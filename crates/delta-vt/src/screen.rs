//! Screen model. Shapes are transcribed from ghostty v1.4.0
//! (`Terminal`, `Screen`, `PageList`, `Page`) with the vertical scrolling
//! semantics of tmux/xterm. No scrollback on the alternate screen and no
//! reflow there, matching ghostty's `resize_pull_scrollback=false` behavior
//! that ConPTY requires.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

use crate::parser::{Action, Csi, Parser};

pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_DIM: u16 = 1 << 1;
pub const ATTR_ITALIC: u16 = 1 << 2;
pub const ATTR_UNDERLINE: u16 = 1 << 3;
pub const ATTR_BLINK: u16 = 1 << 4;
pub const ATTR_REVERSE: u16 = 1 << 5;
pub const ATTR_STRIKE: u16 = 1 << 6;
pub const ATTR_INVISIBLE: u16 = 1 << 7;
pub const ATTR_DOUBLE_UNDERLINE: u16 = 1 << 8;
pub const ATTR_CURLY_UNDERLINE: u16 = 1 << 9;
pub const ATTR_DOTTED_UNDERLINE: u16 = 1 << 10;
pub const ATTR_DASHED_UNDERLINE: u16 = 1 << 11;
pub const ATTR_OVERLINE: u16 = 1 << 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
	Default,
	Indexed(u8),
	Rgb(u8, u8, u8),
}

impl Default for Color {
	fn default() -> Self {
		Color::Default
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
	pub ch: char,
	pub comb: char,
	pub has_comb: bool,
	pub fg: Color,
	pub bg: Color,
	pub attrs: u16,
	/// 0 = narrow, 1 = first half of a wide glyph, 2 = trailing half.
	pub wide: u8,
	pub link: u16,
}

impl Default for Cell {
	fn default() -> Self {
		Self {
			ch: ' ',
			comb: '\0',
			has_comb: false,
			fg: Color::Default,
			bg: Color::Default,
			attrs: 0,
			wide: 0,
			link: 0,
		}
	}
}

impl Cell {
	pub fn blank_with(bg: Color) -> Self {
		Self { bg, ..Self::default() }
	}

	pub fn is_blank(&self) -> bool {
		self.ch == ' ' && !self.has_comb && self.wide == 0 && self.bg == Color::Default
	}

	pub fn text(&self) -> String {
		if self.wide == 2 {
			return String::new();
		}
		if self.has_comb {
			format!("{}{}", self.ch, self.comb)
		} else {
			self.ch.to_string()
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pen {
	pub fg: Color,
	pub bg: Color,
	pub attrs: u16,
	pub underline_color: Color,
	pub link: u16,
}

impl Default for Pen {
	fn default() -> Self {
		Self {
			fg: Color::Default,
			bg: Color::Default,
			attrs: 0,
			underline_color: Color::Default,
			link: 0,
		}
	}
}

#[derive(Clone, Debug)]
pub struct Row {
	pub cells: Vec<Cell>,
	/// This row continues on the next row (soft wrap).
	pub wrapped: bool,
}

impl Row {
	fn new(cols: usize) -> Self {
		Self { cells: vec![Cell::default(); cols], wrapped: false }
	}

	fn trim_end(&mut self) {
		while let Some(last) = self.cells.last() {
			if last.is_blank() {
				self.cells.pop();
			} else {
				break;
			}
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseMode {
	#[default]
	None,
	X10,
	Normal,
	Button,
	Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseEncoding {
	#[default]
	X10,
	Utf8,
	Sgr,
	Urxvt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modes {
	pub insert: bool,
	pub origin: bool,
	pub autowrap: bool,
	pub cursor_keys: bool,
	pub keypad_app: bool,
	pub newline_mode: bool,
	pub bracketed_paste: bool,
	pub focus_events: bool,
	pub mouse_mode: MouseMode,
	pub mouse_encoding: MouseEncoding,
	pub win32_input: bool,
	pub kitty_flags: u8,
	pub cursor_visible: bool,
	pub reverse_video: bool,
	pub cursor_blink: bool,
	pub alt_scroll: bool,
}

impl Default for Modes {
	fn default() -> Self {
		Self {
			insert: false,
			origin: false,
			autowrap: true,
			cursor_keys: false,
			keypad_app: false,
			newline_mode: false,
			bracketed_paste: false,
			focus_events: false,
			mouse_mode: MouseMode::None,
			mouse_encoding: MouseEncoding::X10,
			win32_input: false,
			kitty_flags: 0,
			cursor_visible: true,
			reverse_video: false,
			cursor_blink: false,
			alt_scroll: false,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
	Ascii,
	LineDrawing,
}

#[derive(Clone, Debug)]
struct SavedCursor {
	x: usize,
	y: usize,
	pen: Pen,
	origin: bool,
	autowrap: bool,
	charset: [Charset; 2],
	gl: usize,
}

#[derive(Clone, Debug)]
struct Grid {
	rows: Vec<Row>,
	cursor_x: usize,
	cursor_y: usize,
	saved: Option<SavedCursor>,
	top: usize,
	bottom: usize,
	wrap_pending: bool,
}

impl Grid {
	fn new(cols: usize, rows: usize) -> Self {
		Self {
			rows: (0..rows).map(|_| Row::new(cols)).collect(),
			cursor_x: 0,
			cursor_y: 0,
			saved: None,
			top: 0,
			bottom: rows.saturating_sub(1),
			wrap_pending: false,
		}
	}

	fn resize_no_reflow(&mut self, cols: usize, rows: usize) {
		for row in &mut self.rows {
			row.cells.resize(cols, Cell::default());
			row.cells.truncate(cols);
			if row.cells.iter().all(|c| c.is_blank()) {
				row.wrapped = false;
			}
		}
		self.rows.resize_with(rows, || Row::new(cols));
		self.rows.truncate(rows);
		self.cursor_x = self.cursor_x.min(cols.saturating_sub(1));
		self.cursor_y = self.cursor_y.min(rows.saturating_sub(1));
		self.top = 0;
		self.bottom = rows.saturating_sub(1);
		self.wrap_pending = false;
		if let Some(s) = &mut self.saved {
			s.x = s.x.min(cols.saturating_sub(1));
			s.y = s.y.min(rows.saturating_sub(1));
		}
	}
}

pub struct Screen {
	pub cols: usize,
	pub rows: usize,
	primary: Grid,
	alt: Grid,
	active_alt: bool,
	pub pen: Pen,
	charset: [Charset; 2],
	gl: usize,
	pub modes: Modes,
	pub title: String,
	pub cwd: Option<String>,
	pub cursor_style: u8,
	pub sync_updates: bool,
	pub bell: bool,
	pub dirty: bool,
	pub default_fg: Color,
	pub default_bg: Color,
	palette: [Option<Color>; 256],
	links: Vec<String>,
	scrollback: VecDeque<Row>,
	scrollback_limit: usize,
	tabs: Vec<bool>,
	parser: Parser,
	response: Vec<u8>,
	last_graphic: Option<char>,
	kitty_stack: Vec<u8>,
}

impl Screen {
	pub fn new(cols: usize, rows: usize) -> Self {
		let cols = cols.max(1);
		let rows = rows.max(1);
		let mut tabs = vec![false; cols];
		for (i, t) in tabs.iter_mut().enumerate() {
			*t = i % 8 == 0;
		}
		Self {
			cols,
			rows,
			primary: Grid::new(cols, rows),
			alt: Grid::new(cols, rows),
			active_alt: false,
			pen: Pen::default(),
			charset: [Charset::Ascii; 2],
			gl: 0,
			modes: Modes::default(),
			title: String::new(),
			cwd: None,
			cursor_style: 0,
			sync_updates: false,
			bell: false,
			dirty: true,
			default_fg: Color::Rgb(220, 224, 232),
			default_bg: Color::Rgb(20, 22, 28),
			palette: [None; 256],
			links: vec![String::new()],
			scrollback: VecDeque::new(),
			scrollback_limit: 10_000,
			tabs,
			parser: Parser::new(),
			response: Vec::new(),
			last_graphic: None,
			kitty_stack: Vec::new(),
		}
	}

	pub fn set_scrollback_limit(&mut self, lines: usize) {
		self.scrollback_limit = lines;
		while self.scrollback.len() > self.scrollback_limit {
			self.scrollback.pop_front();
		}
	}

	pub fn feed(&mut self, bytes: &[u8]) {
		for &b in bytes {
			let acts = self.parser.next(b);
			for a in acts.into_iter().flatten() {
				self.perform(a);
			}
		}
		self.dirty = true;
	}

	pub fn take_response(&mut self) -> Vec<u8> {
		std::mem::take(&mut self.response)
	}

	fn respond(&mut self, s: &[u8]) {
		if self.response.len() < 1 << 20 {
			self.response.extend_from_slice(s);
		}
	}

	fn grid(&self) -> &Grid {
		if self.active_alt { &self.alt } else { &self.primary }
	}

	fn grid_mut(&mut self) -> &mut Grid {
		if self.active_alt { &mut self.alt } else { &mut self.primary }
	}

	pub fn is_alt(&self) -> bool {
		self.active_alt
	}

	pub fn cursor(&self) -> (usize, usize) {
		(self.grid().cursor_x, self.grid().cursor_y)
	}

	pub fn cursor_abs_row(&self) -> usize {
		if self.active_alt {
			self.grid().cursor_y
		} else {
			self.scrollback.len() + self.grid().cursor_y
		}
	}

	pub fn scrollback_len(&self) -> usize {
		self.scrollback.len()
	}

	pub fn total_lines(&self) -> usize {
		if self.active_alt {
			self.alt.rows.len()
		} else {
			self.scrollback.len() + self.primary.rows.len()
		}
	}

	/// Absolute row across scrollback + primary screen.
	pub fn row_abs(&self, idx: usize) -> Option<&Row> {
		let sb = self.scrollback.len();
		if idx < sb {
			self.scrollback.get(idx)
		} else {
			self.primary.rows.get(idx - sb)
		}
	}

	pub fn alt_row(&self, idx: usize) -> Option<&Row> {
		self.alt.rows.get(idx)
	}

	pub fn line_text(&self, idx: usize) -> Option<String> {
		let row = self.row_abs(idx)?;
		let mut s = String::with_capacity(row.cells.len());
		for c in &row.cells {
			if c.wide == 2 {
				continue;
			}
			s.push(c.ch);
			if c.has_comb {
				s.push(c.comb);
			}
		}
		while s.ends_with(' ') {
			s.pop();
		}
		Some(s)
	}

	pub fn all_text(&self) -> String {
		let total = self.scrollback.len() + self.primary.rows.len();
		let mut out = String::new();
		for i in 0..total {
			if let Some(line) = self.line_text(i) {
				out.push_str(&line);
				out.push('\n');
			}
		}
		out
	}

	pub fn alt_text(&self) -> String {
		let mut out = String::new();
		for row in &self.alt.rows {
			let mut s: String = row.cells.iter().filter(|c| c.wide != 2).map(|c| c.ch).collect();
			while s.ends_with(' ') {
				s.pop();
			}
			out.push_str(&s);
			out.push('\n');
		}
		out
	}

	pub fn text_between(
		&self,
		start_row: usize,
		start_col: usize,
		end_row: usize,
		end_col: usize,
	) -> String {
		let (sr, sc, er, ec) = if (start_row, start_col) <= (end_row, end_col) {
			(start_row, start_col, end_row, end_col)
		} else {
			(end_row, end_col, start_row, start_col)
		};
		let mut out = String::new();
		for r in sr..=er {
			let Some(row) = self.row_abs(r) else { continue };
			let from = if r == sr { sc.min(row.cells.len()) } else { 0 };
			let to = if r == er { (ec + 1).min(row.cells.len()) } else { row.cells.len() };
			for c in &row.cells[from..to] {
				if c.wide == 2 {
					continue;
				}
				out.push(c.ch);
				if c.has_comb {
					out.push(c.comb);
				}
			}
			if r != er {
				out.push('\n');
			}
		}
		out
	}

	pub fn palette_color(&self, idx: u8) -> Color {
		if let Some(c) = self.palette[idx as usize] {
			return c;
		}
		let (r, g, b) = xterm_palette(idx);
		Color::Rgb(r, g, b)
	}

	pub fn link(&self, id: u16) -> Option<&str> {
		if id == 0 {
			None
		} else {
			self.links.get(id as usize).map(|s| s.as_str())
		}
	}

	pub fn visible_top(&self) -> usize {
		if self.active_alt {
			0
		} else {
			(self.scrollback.len() + self.primary.rows.len()).saturating_sub(self.rows)
		}
	}

	// ---------------------------------------------------------------- input

	fn perform(&mut self, a: Action) {
		match a {
			Action::Print(c) => self.print(c),
			Action::Execute(b) => self.execute(b),
			Action::Csi(c) => self.csi(c),
			Action::Esc { intermediates, final_byte } => self.esc(&intermediates, final_byte),
			Action::Osc(data) => self.osc(&data),
			Action::DcsHook(_) | Action::DcsPut(_) | Action::DcsUnhook => {}
			Action::ApcStart | Action::ApcPut(_) | Action::ApcEnd => {}
		}
	}

	fn mapped_char(&self, c: char) -> char {
		if self.charset[self.gl] == Charset::LineDrawing {
			line_drawing(c)
		} else {
			c
		}
	}

	fn print(&mut self, c: char) {
		let c = self.mapped_char(c);
		if c == ' ' {
			return;
		}
		let width = UnicodeWidthChar::width(c).unwrap_or(0);
		if width == 0 {
			let (x, y) = (self.grid().cursor_x, self.grid().cursor_y);
			if x > 0 {
				let cell = &mut self.grid_mut().rows[y].cells[x - 1];
				if cell.wide != 2 && !cell.has_comb {
					cell.comb = c;
					cell.has_comb = true;
				}
			}
			return;
		}
		let cols = self.cols;
		let autowrap = self.modes.autowrap;
		if self.grid().wrap_pending {
			self.grid_mut().wrap_pending = false;
			if autowrap {
				let y = self.grid().cursor_y;
				self.grid_mut().rows[y].wrapped = true;
				self.grid_mut().cursor_x = 0;
				self.linefeed();
			}
		}
		let x = self.grid().cursor_x;
		if width == 2 && x + 2 > cols {
			if autowrap {
				let y = self.grid().cursor_y;
				self.grid_mut().rows[y].wrapped = true;
				self.grid_mut().cursor_x = 0;
				self.linefeed();
			} else {
				self.grid_mut().cursor_x = cols.saturating_sub(1);
			}
		}
		let x = self.grid().cursor_x;
		let y = self.grid().cursor_y;
		if self.modes.insert {
			let row = &mut self.grid_mut().rows[y];
			if !row.cells.is_empty() {
				let at = x.min(row.cells.len() - 1);
				for _ in 0..width {
					row.cells.insert(at, Cell::default());
				}
				row.cells.truncate(cols);
			}
		}
		self.clear_wide_at(x, y);
		let pen = self.pen;
		let link = pen.link;
		self.set_cell(
			x,
			y,
			Cell {
				ch: c,
				fg: pen.fg,
				bg: pen.bg,
				attrs: pen.attrs,
				wide: if width == 2 { 1 } else { 0 },
				link,
				..Cell::default()
			},
		);
		if width == 2 {
			self.set_cell(
				x + 1,
				y,
				Cell {
					ch: ' ',
					fg: pen.fg,
					bg: pen.bg,
					attrs: pen.attrs,
					wide: 2,
					link,
					..Cell::default()
				},
			);
		}
		let g = self.grid_mut();
		if g.cursor_x + width >= cols {
			g.cursor_x = cols.saturating_sub(1);
			g.wrap_pending = autowrap;
		} else {
			g.cursor_x += width;
		}
		self.last_graphic = Some(c);
	}

	fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
		let g = self.grid_mut();
		if let Some(row) = g.rows.get_mut(y) {
			if let Some(slot) = row.cells.get_mut(x) {
				*slot = cell;
			}
		}
	}

	fn clear_wide_at(&mut self, x: usize, y: usize) {
		let g = self.grid_mut();
		let Some(row) = g.rows.get_mut(y) else { return };
		if let Some(c) = row.cells.get(x).copied() {
			match c.wide {
				1 => {
					if let Some(t) = row.cells.get_mut(x + 1) {
						if t.wide == 2 {
							*t = Cell::default();
						}
					}
				}
				2 => {
					if x > 0 {
						if let Some(h) = row.cells.get_mut(x - 1) {
							if h.wide == 1 {
								*h = Cell::default();
							}
						}
					}
				}
				_ => {}
			}
		}
	}

	fn execute(&mut self, b: u8) {
		match b {
			0x07 => self.bell = true,
			0x08 => {
				let g = self.grid_mut();
				if g.wrap_pending {
					g.wrap_pending = false;
				} else if g.cursor_x > 0 {
					g.cursor_x -= 1;
				}
			}
			0x09 => self.tab_forward(1),
			0x0A | 0x0B | 0x0C => {
				if self.modes.newline_mode {
					self.grid_mut().cursor_x = 0;
				}
				self.linefeed();
			}
			0x0D => {
				let g = self.grid_mut();
				g.cursor_x = 0;
				g.wrap_pending = false;
			}
			0x0E => self.gl = 1,
			0x0F => self.gl = 0,
			_ => {}
		}
	}

	fn tab_forward(&mut self, n: usize) {
		let cols = self.cols;
		let mut x = self.grid().cursor_x;
		for _ in 0..n {
			x += 1;
			while x < cols && !self.tabs.get(x).copied().unwrap_or(false) {
				x += 1;
			}
			x = x.min(cols.saturating_sub(1));
		}
		let g = self.grid_mut();
		g.cursor_x = x;
		g.wrap_pending = false;
	}

	fn tab_backward(&mut self, n: usize) {
		let mut x = self.grid().cursor_x;
		for _ in 0..n {
			while x > 0 {
				x -= 1;
				if self.tabs.get(x).copied().unwrap_or(false) {
					break;
				}
			}
		}
		let g = self.grid_mut();
		g.cursor_x = x;
		g.wrap_pending = false;
	}

	fn linefeed(&mut self) {
		let bottom = self.grid().bottom;
		let y = self.grid().cursor_y;
		let rows = self.rows;
		{
			let g = self.grid_mut();
			g.wrap_pending = false;
			if y < bottom && y + 1 < rows {
				g.cursor_y += 1;
			}
		}
		if y == bottom {
			self.scroll_up(1);
		}
	}

	fn reverse_index(&mut self) {
		let top = self.grid().top;
		let y = self.grid().cursor_y;
		{
			let g = self.grid_mut();
			g.wrap_pending = false;
			if y > top && y > 0 {
				g.cursor_y -= 1;
			}
		}
		if y == top {
			self.scroll_down(1);
		}
	}

	fn scroll_up(&mut self, n: usize) {
		let (top, bottom) = (self.grid().top, self.grid().bottom);
		let full = top == 0 && bottom + 1 == self.rows;
		let primary = !self.active_alt;
		for _ in 0..n.min(self.rows.max(1)) {
			let g = self.grid_mut();
			if top >= g.rows.len() {
				break;
			}
			let removed = g.rows.remove(top);
			if full && primary {
				let mut row = removed;
				if row.cells.iter().all(|c| c.is_blank()) {
					row.wrapped = false;
				}
				if self.scrollback_limit > 0 {
					self.scrollback.push_back(row);
					while self.scrollback.len() > self.scrollback_limit {
						self.scrollback.pop_front();
					}
				}
			}
			let cols = self.cols;
			let g = self.grid_mut();
			let insert_at = bottom.min(g.rows.len());
			g.rows.insert(insert_at, Row::new(cols));
		}
	}

	fn scroll_down(&mut self, n: usize) {
		let (top, bottom) = (self.grid().top, self.grid().bottom);
		let full = top == 0 && bottom + 1 == self.rows;
		let primary = !self.active_alt;
		let cols = self.cols;
		let rows_len = self.rows;
		for _ in 0..n.min(rows_len.max(1)) {
			let mut restored = if full && primary { self.scrollback.pop_back() } else { None };
			if let Some(r) = &mut restored {
				r.cells.resize(cols, Cell::default());
			}
			let g = self.grid_mut();
			if top >= g.rows.len() {
				break;
			}
			let row = restored.unwrap_or_else(|| Row::new(cols));
			g.rows.insert(top.min(g.rows.len()), row);
			if g.rows.len() > rows_len && bottom + 1 < g.rows.len() {
				g.rows.remove(bottom + 1);
			}
		}
	}

	fn blank_row(&mut self, y: usize) {
		if y < self.rows {
			let bg = self.pen.bg;
			let cols = self.cols;
			let g = self.grid_mut();
			g.rows[y] = Row { cells: vec![Cell::blank_with(bg); cols], wrapped: false };
		}
	}

	fn erase_to_end(&mut self, x: usize, y: usize, bg: Color) {
		let cols = self.cols;
		let g = self.grid_mut();
		if let Some(row) = g.rows.get_mut(y) {
			for cell in row.cells.iter_mut().skip(x.min(cols)) {
				*cell = Cell::blank_with(bg);
			}
			row.wrapped = false;
		}
	}

	fn erase_to_start(&mut self, x: usize, y: usize, bg: Color) {
		let g = self.grid_mut();
		if let Some(row) = g.rows.get_mut(y) {
			let end = (x + 1).min(row.cells.len());
			for cell in row.cells.iter_mut().take(end) {
				*cell = Cell::blank_with(bg);
			}
			if end == row.cells.len() {
				row.wrapped = false;
			}
		}
	}

	fn clear_grid(&mut self) {
		let cols = self.cols;
		let rows = self.rows;
		let bg = self.pen.bg;
		let g = self.grid_mut();
		for row in &mut g.rows {
			*row = Row { cells: vec![Cell::blank_with(bg); cols], wrapped: false };
		}
		g.rows.resize_with(rows, || Row::new(cols));
		g.cursor_x = 0;
		g.cursor_y = 0;
		g.wrap_pending = false;
		g.top = 0;
		g.bottom = rows.saturating_sub(1);
	}

	fn home(&mut self) {
		let (top, origin) = (self.grid().top, self.modes.origin);
		let rows = self.rows;
		let g = self.grid_mut();
		g.cursor_x = 0;
		g.cursor_y = if origin { top } else { 0 };
		g.wrap_pending = false;
		if g.cursor_y >= rows {
			g.cursor_y = rows.saturating_sub(1);
		}
	}

	fn csi(&mut self, c: Csi) {
		if c.private() && c.final_byte != b'h' && c.final_byte != b'l' && c.final_byte != b'n' {
			// Most private sequences are still valid only for a few finals.
			match (c.final_byte, c.has(b'$')) {
				(b'p', true) => {
					self.decrqm(&c);
					return;
				}
				_ => {}
			}
		}
		match c.final_byte {
			b'@' => {
				let n = c.param(0, 1) as usize;
				let (x, y) = (self.grid().cursor_x, self.grid().cursor_y);
				let cols = self.cols;
				let g = self.grid_mut();
				if let Some(row) = g.rows.get_mut(y) {
					for _ in 0..n {
						row.cells.insert(x.min(row.cells.len()), Cell::default());
					}
					row.cells.truncate(cols);
				}
			}
			b'A' => {
				let n = c.param(0, 1) as usize;
				let top = self.grid().top;
				let y = self.grid().cursor_y;
				let floor = if y >= top { top } else { 0 };
				self.grid_mut().cursor_y = y.saturating_sub(n).max(floor);
				self.grid_mut().wrap_pending = false;
			}
			b'B' => {
				let n = c.param(0, 1) as usize;
				let bottom = self.grid().bottom;
				let y = self.grid().cursor_y;
				let ceil = if y <= bottom { bottom } else { self.rows - 1 };
				self.grid_mut().cursor_y = (y + n).min(ceil);
				self.grid_mut().wrap_pending = false;
			}
			b'C' => {
				let n = c.param(0, 1) as usize;
				let cols = self.cols;
				let g = self.grid_mut();
				g.cursor_x = (g.cursor_x + n).min(cols - 1);
				g.wrap_pending = false;
			}
			b'D' => {
				let n = c.param(0, 1) as usize;
				let g = self.grid_mut();
				g.cursor_x = g.cursor_x.saturating_sub(n);
				g.wrap_pending = false;
			}
			b'E' => {
				let n = c.param(0, 1) as usize;
				let bottom = self.grid().bottom;
				let y = self.grid().cursor_y;
				let ceil = if y <= bottom { bottom } else { self.rows - 1 };
				let g = self.grid_mut();
				g.cursor_y = (y + n).min(ceil);
				g.cursor_x = 0;
				g.wrap_pending = false;
			}
			b'F' => {
				let n = c.param(0, 1) as usize;
				let top = self.grid().top;
				let y = self.grid().cursor_y;
				let floor = if y >= top { top } else { 0 };
				let g = self.grid_mut();
				g.cursor_y = y.saturating_sub(n).max(floor);
				g.cursor_x = 0;
				g.wrap_pending = false;
			}
			b'G' | b'`' => {
				let n = c.param(0, 1) as usize;
				let cols = self.cols;
				let g = self.grid_mut();
				g.cursor_x = (n.saturating_sub(1)).min(cols - 1);
				g.wrap_pending = false;
			}
			b'H' | b'f' => {
				let row = c.param(0, 1) as usize;
				let col = c.param(1, 1) as usize;
				let (top, bottom, origin) = (self.grid().top, self.grid().bottom, self.modes.origin);
				let cols = self.cols;
				let rows = self.rows;
				let g = self.grid_mut();
				if origin {
					g.cursor_y = (top + row.saturating_sub(1)).min(bottom);
					g.cursor_x = col.saturating_sub(1).min(cols - 1);
				} else {
					g.cursor_y = row.saturating_sub(1).min(rows - 1);
					g.cursor_x = col.saturating_sub(1).min(cols - 1);
				}
				g.wrap_pending = false;
			}
			b'I' => {
				let n = c.param(0, 1) as usize;
				self.tab_forward(n);
			}
			b'J' => {
				let n = c.raw(0);
				let (x, y) = (self.grid().cursor_x, self.grid().cursor_y);
				let bg = self.pen.bg;
				match n {
					0 => {
						self.erase_to_end(x, y, bg);
						for yy in (y + 1)..self.rows {
							self.blank_row(yy);
						}
					}
					1 => {
						self.erase_to_start(x, y, bg);
						for yy in 0..y {
							self.blank_row(yy);
						}
					}
					2 => {
						for yy in 0..self.rows {
							self.blank_row(yy);
						}
						self.grid_mut().wrap_pending = false;
					}
					3 => {
						self.scrollback.clear();
						for yy in 0..self.rows {
							self.blank_row(yy);
						}
					}
					_ => {}
				}
			}
			b'K' => {
				let n = c.raw(0);
				let (x, y) = (self.grid().cursor_x, self.grid().cursor_y);
				let bg = self.pen.bg;
				match n {
					0 => self.erase_to_end(x, y, bg),
					1 => self.erase_to_start(x, y, bg),
					2 => {
						self.blank_row(y);
					}
					_ => {}
				}
			}
			b'L' => {
				let n = c.param(0, 1) as usize;
				let y = self.grid().cursor_y;
				let (top, bottom) = (self.grid().top, self.grid().bottom);
				if y >= top && y <= bottom {
					let cols = self.cols;
					let g = self.grid_mut();
					for _ in 0..n {
						g.rows.insert(y, Row::new(cols));
					}
					for _ in 0..n {
						if bottom + 1 < g.rows.len() {
							g.rows.remove(bottom + 1);
						}
					}
				}
			}
			b'M' => {
				let n = c.param(0, 1) as usize;
				let y = self.grid().cursor_y;
				let (top, bottom) = (self.grid().top, self.grid().bottom);
				if y >= top && y <= bottom {
					let cols = self.cols;
					let g = self.grid_mut();
					for _ in 0..n {
						if y < g.rows.len() {
							g.rows.remove(y);
						}
						if bottom <= g.rows.len() {
							g.rows.insert(bottom.min(g.rows.len()), Row::new(cols));
						}
					}
				}
			}
			b'P' => {
				let n = c.param(0, 1) as usize;
				let (x, y) = (self.grid().cursor_x, self.grid().cursor_y);
				let g = self.grid_mut();
				if let Some(row) = g.rows.get_mut(y) {
					for _ in 0..n {
						if x < row.cells.len() {
							row.cells.remove(x);
							row.cells.push(Cell::default());
						}
					}
				}
			}
			b'S' => {
				self.scroll_up(c.param(0, 1) as usize);
			}
			b'T' => {
				self.scroll_down(c.param(0, 1) as usize);
			}
			b'X' => {
				let n = c.param(0, 1) as usize;
				let (x, y) = (self.grid().cursor_x, self.grid().cursor_y);
				let bg = self.pen.bg;
				let g = self.grid_mut();
				if let Some(row) = g.rows.get_mut(y) {
					for cell in row.cells.iter_mut().skip(x).take(n) {
						*cell = Cell::blank_with(bg);
					}
				}
			}
			b'Z' => {
				self.tab_backward(c.param(0, 1) as usize);
			}
			b'a' => {
				let n = c.param(0, 1) as usize;
				let cols = self.cols;
				let g = self.grid_mut();
				g.cursor_x = (g.cursor_x + n).min(cols - 1);
				g.wrap_pending = false;
			}
			b'b' => {
				if let Some(ch) = self.last_graphic {
					let n = (c.param(0, 1) as usize).min(self.cols * self.rows);
					for _ in 0..n {
						self.print(ch);
					}
				}
			}
			b'c' => {
				if c.has(b'>') {
					self.respond(b"\x1b[>0;0;0c");
				} else {
					self.respond(b"\x1b[?62;4;6;22c");
				}
			}
			b'd' => {
				let n = c.param(0, 1) as usize;
				let (top, origin, bottom) = (self.grid().top, self.modes.origin, self.grid().bottom);
				let rows = self.rows;
				let g = self.grid_mut();
				g.cursor_y = if origin {
					(top + n.saturating_sub(1)).min(bottom)
				} else {
					n.saturating_sub(1).min(rows - 1)
				};
				g.wrap_pending = false;
			}
			b'e' => {
				let n = c.param(0, 1) as usize;
				let rows = self.rows;
				let g = self.grid_mut();
				g.cursor_y = (g.cursor_y + n).min(rows - 1);
				g.wrap_pending = false;
			}
			b'g' => {
				let n = c.raw(0);
				match n {
					0 => {
						let x = self.grid().cursor_x;
						if x < self.tabs.len() {
							self.tabs[x] = false;
						}
					}
					3 => {
						for t in &mut self.tabs {
							*t = false;
						}
					}
					_ => {}
				}
			}
			b'h' | b'l' => {
				let set = c.final_byte == b'h';
				if c.private() {
					for &p in &c.params {
						self.set_private_mode(p, set);
					}
				} else {
					for &p in &c.params {
						match p {
							4 => self.modes.insert = set,
							20 => self.modes.newline_mode = set,
							12 => self.modes.cursor_blink = set,
							_ => {}
						}
					}
				}
			}
			b'm' => self.sgr(&c),
			b'n' => {
				if c.private() {
					if c.raw(0) == 6 {
						let (x, y) = self.cursor();
						let s = format!("\x1b[?{};{}R", y + 1, x + 1);
						self.respond(s.as_bytes());
					}
				} else {
					match c.raw(0) {
						5 => self.respond(b"\x1b[0n"),
						6 => {
							let (x, y) = self.cursor();
							let s = format!("\x1b[{};{}R", y + 1, x + 1);
							self.respond(s.as_bytes());
						}
						_ => {}
					}
				}
			}
			b'q' => {
				if c.has(b' ') {
					self.cursor_style = c.raw(0) as u8;
				}
			}
			b'r' => {
				let top = c.param(0, 1) as usize;
				let bottom = c.param(1, self.rows as u16) as usize;
				let rows = self.rows;
				let origin = self.modes.origin;
				if top < bottom && bottom <= rows {
					let g = self.grid_mut();
					g.top = top - 1;
					g.bottom = bottom - 1;
					g.cursor_x = 0;
					g.cursor_y = if origin { g.top } else { 0 };
					g.wrap_pending = false;
				}
			}
			b's' => {
				if c.intermediates.is_empty() {
					self.save_cursor();
				}
			}
			b't' => {
				if c.raw(0) == 18 {
					let s = format!("\x1b[8;{};{}t", self.rows, self.cols);
					self.respond(s.as_bytes());
				}
			}
			b'u' => {
				if c.has(b'>') {
					self.kitty_stack.push(self.modes.kitty_flags);
					self.modes.kitty_flags = c.raw(0) as u8;
				} else if c.has(b'<') {
					self.modes.kitty_flags = self.kitty_stack.pop().unwrap_or(0);
				} else if c.has(b'=') {
					let flags = c.raw(0) as u8;
					let mode = c.raw(1);
					match mode {
						1 => self.modes.kitty_flags = flags,
						2 => self.modes.kitty_flags |= flags,
						3 => self.modes.kitty_flags &= !flags,
						_ => {}
					}
				} else if c.private() {
					let s = format!("\x1b[?{}u", self.modes.kitty_flags);
					self.respond(s.as_bytes());
				} else if c.intermediates.is_empty() {
					self.restore_cursor();
				}
			}
			b'x' => {
				if c.raw(0) == 0 {
					self.respond(b"\x1b[2;1;1;112;112;1;0x");
				}
			}
			b'p' => {
				if c.has(b'!') {
					self.soft_reset();
				}
			}
			_ => {}
		}
	}

	fn decrqm(&mut self, c: &Csi) {
		let mode = c.raw(0);
		let set = self.mode_is_set(mode);
		let s = format!("\x1b[?{};{};$y", mode, if set { 1 } else { 2 });
		self.respond(s.as_bytes());
	}

	fn mode_is_set(&self, mode: u16) -> bool {
		match mode {
			1 => self.modes.cursor_keys,
			5 => self.modes.reverse_video,
			6 => self.modes.origin,
			7 => self.modes.autowrap,
			12 => self.modes.cursor_blink,
			25 => self.modes.cursor_visible,
			1000 => self.modes.mouse_mode == MouseMode::Normal,
			1002 => self.modes.mouse_mode == MouseMode::Button,
			1003 => self.modes.mouse_mode == MouseMode::Any,
			1004 => self.modes.focus_events,
			1006 => self.modes.mouse_encoding == MouseEncoding::Sgr,
			1015 => self.modes.mouse_encoding == MouseEncoding::Urxvt,
			2004 => self.modes.bracketed_paste,
			2026 => self.sync_updates,
			9001 => self.modes.win32_input,
			1047 | 1049 => self.active_alt,
			47 => self.active_alt,
			_ => false,
		}
	}

	fn set_private_mode(&mut self, mode: u16, set: bool) {
		match mode {
			1 => self.modes.cursor_keys = set,
			5 => self.modes.reverse_video = set,
			6 => {
				self.modes.origin = set;
				self.home();
			}
			7 => self.modes.autowrap = set,
			12 => self.modes.cursor_blink = set,
			25 => self.modes.cursor_visible = set,
			1000 => self.modes.mouse_mode = if set { MouseMode::Normal } else { MouseMode::None },
			1002 => self.modes.mouse_mode = if set { MouseMode::Button } else { MouseMode::None },
			1003 => self.modes.mouse_mode = if set { MouseMode::Any } else { MouseMode::None },
			1004 => self.modes.focus_events = set,
			1005 => self.modes.mouse_encoding = if set { MouseEncoding::Utf8 } else { MouseEncoding::X10 },
			1006 => self.modes.mouse_encoding = if set { MouseEncoding::Sgr } else { MouseEncoding::X10 },
			1015 => self.modes.mouse_encoding = if set { MouseEncoding::Urxvt } else { MouseEncoding::X10 },
			1047 => {
				if set {
					self.enter_alt(false, true);
				} else {
					self.leave_alt(false);
				}
			}
			1049 => {
				if set {
					self.enter_alt(true, true);
				} else {
					self.leave_alt(true);
				}
			}
			47 => {
				if set {
					self.enter_alt(false, true);
				} else {
					self.leave_alt(false);
				}
			}
			1048 => {
				if set {
					self.save_cursor();
				} else {
					self.restore_cursor();
				}
			}
			1007 => self.modes.alt_scroll = set,
			2004 => self.modes.bracketed_paste = set,
			2026 => self.sync_updates = set,
			9001 => self.modes.win32_input = set,
			_ => {}
		}
	}

	fn enter_alt(&mut self, save_cursor: bool, clear: bool) {
		if self.active_alt {
			return;
		}
		if save_cursor {
			self.save_cursor();
		}
		if clear {
			self.alt = Grid::new(self.cols, self.rows);
		}
		self.active_alt = true;
	}

	fn leave_alt(&mut self, restore: bool) {
		if !self.active_alt {
			return;
		}
		self.active_alt = false;
		if restore {
			self.restore_cursor();
		}
	}

	fn save_cursor(&mut self) {
		let x = self.grid().cursor_x;
		let y = self.grid().cursor_y;
		let saved = SavedCursor {
			x,
			y,
			pen: self.pen,
			origin: self.modes.origin,
			autowrap: self.modes.autowrap,
			charset: self.charset,
			gl: self.gl,
		};
		self.grid_mut().saved = Some(saved);
	}

	fn restore_cursor(&mut self) {
		let Some(s) = self.grid().saved.clone() else { return };
		let cols = self.cols;
		let rows = self.rows;
		let g = self.grid_mut();
		g.cursor_x = s.x.min(cols - 1);
		g.cursor_y = s.y.min(rows - 1);
		g.wrap_pending = false;
		self.pen = s.pen;
		self.modes.origin = s.origin;
		self.modes.autowrap = s.autowrap;
		self.charset = s.charset;
		self.gl = s.gl;
	}

	fn sgr(&mut self, c: &Csi) {
		let params = &c.params;
		if params.is_empty() {
			self.pen = Pen::default();
			return;
		}
		let mut i = 0usize;
		while i < params.len() {
			let p = params[i];
			match p {
				0 => self.pen = Pen::default(),
				1 => self.pen.attrs |= ATTR_BOLD,
				2 => self.pen.attrs |= ATTR_DIM,
				3 => self.pen.attrs |= ATTR_ITALIC,
				4 => {
					self.pen.attrs &= !(ATTR_UNDERLINE
						| ATTR_DOUBLE_UNDERLINE
						| ATTR_CURLY_UNDERLINE
						| ATTR_DOTTED_UNDERLINE
						| ATTR_DASHED_UNDERLINE);
					if c.colon_after(i) && i + 1 < params.len() {
						match params[i + 1] {
							0 => {}
							2 => self.pen.attrs |= ATTR_DOUBLE_UNDERLINE,
							3 => self.pen.attrs |= ATTR_CURLY_UNDERLINE,
							4 => self.pen.attrs |= ATTR_DOTTED_UNDERLINE,
							5 => self.pen.attrs |= ATTR_DASHED_UNDERLINE,
							_ => self.pen.attrs |= ATTR_UNDERLINE,
						}
						i += 1;
					} else {
						self.pen.attrs |= ATTR_UNDERLINE;
					}
				}
				5 | 6 => self.pen.attrs |= ATTR_BLINK,
				7 => self.pen.attrs |= ATTR_REVERSE,
				8 => self.pen.attrs |= ATTR_INVISIBLE,
				9 => self.pen.attrs |= ATTR_STRIKE,
				21 => {
					self.pen.attrs |= ATTR_DOUBLE_UNDERLINE;
				}
				22 => self.pen.attrs &= !(ATTR_BOLD | ATTR_DIM),
				23 => self.pen.attrs &= !ATTR_ITALIC,
				24 => {
					self.pen.attrs &= !(ATTR_UNDERLINE
						| ATTR_DOUBLE_UNDERLINE
						| ATTR_CURLY_UNDERLINE
						| ATTR_DOTTED_UNDERLINE
						| ATTR_DASHED_UNDERLINE);
				}
				25 => self.pen.attrs &= !ATTR_BLINK,
				27 => self.pen.attrs &= !ATTR_REVERSE,
				28 => self.pen.attrs &= !ATTR_INVISIBLE,
				29 => self.pen.attrs &= !ATTR_STRIKE,
				30..=37 => self.pen.fg = Color::Indexed((p - 30) as u8),
				38 => i = self.extended_color(i, params, c, true),
				39 => self.pen.fg = Color::Default,
				40..=47 => self.pen.bg = Color::Indexed((p - 40) as u8),
				48 => i = self.extended_color(i, params, c, false),
				49 => self.pen.bg = Color::Default,
				53 => self.pen.attrs |= ATTR_OVERLINE,
				55 => self.pen.attrs &= !ATTR_OVERLINE,
				58 => i = self.underline_color(i, params, c),
				59 => self.pen.underline_color = Color::Default,
				90..=97 => self.pen.fg = Color::Indexed((p - 90 + 8) as u8),
				100..=107 => self.pen.bg = Color::Indexed((p - 100 + 8) as u8),
				_ => {}
			}
			i += 1;
		}
	}

	fn extended_color(&mut self, i: usize, params: &[u16], c: &Csi, fg: bool) -> usize {
		if i + 1 >= params.len() {
			return i;
		}
		let kind = params[i + 1];
		let colon = c.colon_after(i);
		let mut j = i + 2;
		let color = if kind == 2 {
			let mut sub = Vec::new();
			while j < params.len() {
				sub.push(params[j]);
				let more = if colon { c.colon_after(j) } else { j + 1 < params.len() && !c.colon_after(j) };
				j += 1;
				if !more {
					break;
				}
			}
			let rgb = if sub.len() >= 4 { &sub[sub.len() - 3..] } else { &sub[..] };
			if rgb.len() >= 3 {
				Color::Rgb(rgb[0].min(255) as u8, rgb[1].min(255) as u8, rgb[2].min(255) as u8)
			} else {
				return j.saturating_sub(1);
			}
		} else if kind == 5 {
			if j < params.len() {
				let idx = params[j].min(255) as u8;
				j += 1;
				Color::Indexed(idx)
			} else {
				return i + 1;
			}
		} else {
			return i + 1;
		};
		if fg {
			self.pen.fg = color;
		} else {
			self.pen.bg = color;
		}
		j.saturating_sub(1)
	}

	fn underline_color(&mut self, i: usize, params: &[u16], _c: &Csi) -> usize {
		if i + 2 < params.len() && params[i + 1] == 5 {
			self.pen.underline_color = Color::Indexed(params[i + 2].min(255) as u8);
			return i + 2;
		}
		if i + 4 < params.len() && params[i + 1] == 2 {
			self.pen.underline_color = Color::Rgb(
				params[i + 2].min(255) as u8,
				params[i + 3].min(255) as u8,
				params[i + 4].min(255) as u8,
			);
			return i + 4;
		}
		i
	}

	fn esc(&mut self, intermediates: &[u8], final_byte: u8) {
		match (intermediates.first().copied(), final_byte) {
			(None, b'7') => self.save_cursor(),
			(None, b'8') => self.restore_cursor(),
			(None, b'D') => self.linefeed(),
			(None, b'E') => {
				self.grid_mut().cursor_x = 0;
				self.linefeed();
			}
			(None, b'M') => self.reverse_index(),
			(None, b'H') => {
				let x = self.grid().cursor_x;
				if x < self.tabs.len() {
					self.tabs[x] = true;
				}
			}
			(None, b'c') => self.hard_reset(),
			(None, b'=') => self.modes.keypad_app = true,
			(None, b'>') => self.modes.keypad_app = false,
			(None, b'Z') => self.respond(b"\x1b[?62;4;6;22c"),
			(Some(b'('), _) => self.charset[0] = charset_for(final_byte),
			(Some(b')'), _) => self.charset[1] = charset_for(final_byte),
			(Some(b'#'), b'8') => {
				let pen = self.pen;
				let g = self.grid_mut();
				for row in &mut g.rows {
					for cell in row.cells.iter_mut() {
						*cell = Cell { ch: 'E', fg: pen.fg, bg: pen.bg, ..Cell::default() };
					}
				}
				g.cursor_x = 0;
				g.cursor_y = 0;
				g.wrap_pending = false;
			}
			_ => {}
		}
	}

	fn osc(&mut self, data: &[u8]) {
		let (cmd, rest) = match data.iter().position(|&b| b == b';') {
			Some(pos) => (&data[..pos], &data[pos + 1..]),
			None => (data, &[][..]),
		};
		let Ok(cmd) = std::str::from_utf8(cmd) else { return };
		match cmd {
			"0" | "2" => {
				self.title = String::from_utf8_lossy(rest).to_string();
			}
			"7" => {
				let s = String::from_utf8_lossy(rest).to_string();
				self.cwd = Some(s.trim_start_matches("file://").to_string());
			}
			"8" => {
				let s = String::from_utf8_lossy(rest);
				if let Some((_, uri)) = s.split_once(';') {
					if uri.is_empty() {
						self.pen.link = 0;
					} else {
						let idx = self.links.len();
						if idx < u16::MAX as usize {
							self.links.push(uri.to_string());
							self.pen.link = idx as u16;
						}
					}
				}
			}
			"4" => {
				let s = String::from_utf8_lossy(rest);
				let mut it = s.split(';');
				if let (Some(idx), Some(spec)) = (it.next(), it.next()) {
					if let (Ok(idx), Some(rgb)) = (idx.parse::<usize>(), parse_color(spec)) {
						if idx < 256 {
							self.palette[idx] = Some(rgb);
						}
					}
				}
			}
			"10" => {
				if let Some(c) = parse_color(&String::from_utf8_lossy(rest)) {
					self.default_fg = c;
				}
			}
			"11" => {
				if let Some(c) = parse_color(&String::from_utf8_lossy(rest)) {
					self.default_bg = c;
				}
			}
			"104" => {
				let s = String::from_utf8_lossy(rest);
				if s.trim().is_empty() {
					self.palette = [None; 256];
				} else {
					for part in s.split(';') {
						if let Ok(i) = part.parse::<usize>() {
							if i < 256 {
								self.palette[i] = None;
							}
						}
					}
				}
			}
			"110" => self.default_fg = Color::Rgb(220, 224, 232),
			"111" => self.default_bg = Color::Rgb(20, 22, 28),
			_ => {}
		}
	}

	fn soft_reset(&mut self) {
		self.pen = Pen::default();
		self.modes.insert = false;
		self.modes.origin = false;
		self.modes.autowrap = true;
		self.modes.cursor_visible = true;
		self.modes.cursor_keys = false;
		self.modes.keypad_app = false;
		self.modes.newline_mode = false;
		self.modes.reverse_video = false;
		self.modes.kitty_flags = 0;
		self.kitty_stack.clear();
		let rows = self.rows;
		let g = self.grid_mut();
		g.top = 0;
		g.bottom = rows - 1;
		g.wrap_pending = false;
	}

	fn hard_reset(&mut self) {
		let (cols, rows) = (self.cols, self.rows);
		self.primary = Grid::new(cols, rows);
		self.alt = Grid::new(cols, rows);
		self.active_alt = false;
		self.scrollback.clear();
		self.pen = Pen::default();
		self.charset = [Charset::Ascii; 2];
		self.gl = 0;
		self.modes = Modes::default();
		self.title.clear();
		self.cwd = None;
		self.cursor_style = 0;
		self.sync_updates = false;
		self.palette = [None; 256];
		self.default_fg = Color::Rgb(220, 224, 232);
		self.default_bg = Color::Rgb(20, 22, 28);
		self.links = vec![String::new()];
		self.kitty_stack.clear();
		self.last_graphic = None;
		self.tabs = (0..cols).map(|i| i % 8 == 0).collect();
		self.parser.reset();
	}

	// --------------------------------------------------------------- resize

	pub fn resize(&mut self, cols: usize, rows: usize) {
		let cols = cols.max(1);
		let rows = rows.max(1);
		if cols == self.cols && rows == self.rows {
			return;
		}
		let old_cols = self.cols;
		self.resize_tabs(old_cols, cols);
		if self.active_alt {
			self.primary.resize_no_reflow(cols, rows);
			self.alt.resize_no_reflow(cols, rows);
			self.cols = cols;
			self.rows = rows;
			return;
		}
		let sb_len = self.scrollback.len();
		let mut all: Vec<Row> = self.scrollback.drain(..).collect();
		all.append(&mut self.primary.rows);
		let cursor_all = sb_len + self.primary.cursor_y;
		let cursor_x = self.primary.cursor_x;

		// Split into logical (unwrapped) lines.
		let mut lines: Vec<Vec<Cell>> = Vec::new();
		let mut cur: Vec<Cell> = Vec::new();
		for row in &all {
			let mut cells = row.cells.clone();
			if !row.wrapped {
				while cells.last().is_some_and(|c| c.is_blank()) {
					cells.pop();
				}
			}
			cur.extend(cells);
			if !row.wrapped {
				lines.push(std::mem::take(&mut cur));
			}
		}
		if !cur.is_empty() || lines.is_empty() {
			lines.push(cur);
		}

		// Find the logical line containing the cursor.
		let mut cursor_line = 0usize;
		let mut cursor_off = 0usize;
		{
			let mut row_i = 0usize;
			for (li, line) in lines.iter().enumerate() {
				let nrows = ((line.len() + old_cols - 1) / old_cols).max(1);
				if cursor_all >= row_i && cursor_all < row_i + nrows {
					cursor_line = li;
					cursor_off = (cursor_all - row_i) * old_cols + cursor_x.min(old_cols - 1);
					break;
				}
				row_i += nrows;
				cursor_line = li + 1;
				cursor_off = 0;
			}
		}

		let mut last_nonempty = 0usize;
		for (i, line) in lines.iter().enumerate() {
			if !line.is_empty() {
				last_nonempty = i;
			}
		}
		let keep = (last_nonempty + 1).max(cursor_line + 1).max(1);
		lines.truncate(keep);

		let mut out: Vec<Row> = Vec::new();
		let mut cursor_new_row = 0usize;
		let mut cursor_new_col = 0usize;
		for (li, line) in lines.iter().enumerate() {
			let base = out.len();
			let mut starts: Vec<usize> = Vec::new();
			let mut idx = 0usize;
			loop {
				starts.push(idx);
				let mut cells: Vec<Cell> = Vec::with_capacity(cols);
				while cells.len() < cols && idx < line.len() {
					let cell = line[idx];
					if cell.wide == 1 && cells.len() + 2 > cols {
						// A wide cell cannot fit here. On a 1-column row take
						// it as-is so the loop advances; otherwise leave the
						// gap and retry on the next row.
						if cells.is_empty() {
							cells.push(cell);
							idx += 1;
						} else {
							cells.push(Cell::default());
						}
						break;
					}
					cells.push(cell);
					idx += 1;
					if cell.wide == 1 && idx < line.len() && cells.len() < cols {
						cells.push(line[idx]);
						idx += 1;
					}
				}
				let more = idx < line.len();
				while cells.len() < cols {
					cells.push(Cell::default());
				}
				out.push(Row { cells, wrapped: more });
				if !more {
					break;
				}
			}
			if li == cursor_line {
				let local = starts.iter().rposition(|&s| s <= cursor_off).unwrap_or(0);
				cursor_new_row = base + local;
				cursor_new_col = cursor_off.saturating_sub(starts[local]).min(cols - 1);
			}
		}

		let total = out.len();
		let mut start = total.saturating_sub(rows);
		if cursor_new_row < start {
			start = cursor_new_row;
		}
		if start > total {
			start = total;
		}
		let rest: Vec<Row> = out.split_off(start);
		self.scrollback = out.into();
		while self.scrollback.len() > self.scrollback_limit {
			self.scrollback.pop_front();
		}
		let mut new_rows = rest;
		while new_rows.len() < rows {
			new_rows.push(Row::new(cols));
		}
		new_rows.truncate(rows);
		if let Some(last) = new_rows.last_mut() {
			last.wrapped = false;
		}
		self.primary.rows = new_rows;
		self.primary.cursor_y = cursor_new_row.saturating_sub(start).min(rows - 1);
		self.primary.cursor_x = cursor_new_col.min(cols - 1);
		self.primary.wrap_pending = false;
		self.primary.top = 0;
		self.primary.bottom = rows - 1;
		self.primary.saved = None;
		self.alt.resize_no_reflow(cols, rows);
		self.cols = cols;
		self.rows = rows;
	}

	fn resize_tabs(&mut self, old_cols: usize, cols: usize) {
		let old = std::mem::take(&mut self.tabs);
		let mut tabs = vec![false; cols];
		for (i, t) in tabs.iter_mut().enumerate() {
			*t = if i < old_cols { old.get(i).copied().unwrap_or(false) } else { i % 8 == 0 };
		}
		self.tabs = tabs;
	}
}

fn charset_for(b: u8) -> Charset {
	match b {
		b'0' => Charset::LineDrawing,
		_ => Charset::Ascii,
	}
}

fn line_drawing(c: char) -> char {
	match c {
		'`' => '◆',
		'a' => '▒',
		'b' => '␉',
		'c' => '␌',
		'd' => '␍',
		'e' => '␊',
		'f' => '°',
		'g' => '±',
		'h' => '␤',
		'i' => '␋',
		'j' => '┘',
		'k' => '┐',
		'l' => '┌',
		'm' => '└',
		'n' => '┼',
		'o' => '⎺',
		'p' => '⎻',
		'q' => '─',
		'r' => '⎼',
		's' => '⎽',
		't' => '├',
		'u' => '┤',
		'v' => '┴',
		'w' => '┬',
		'x' => '│',
		'y' => '≤',
		'z' => '≥',
		'{' => 'π',
		'|' => '≠',
		'}' => '£',
		'~' => '·',
		_ => c,
	}
}

fn parse_color(spec: &str) -> Option<Color> {
	let spec = spec.trim();
	if let Some(hex) = spec.strip_prefix('#') {
		if hex.len() == 6 {
			let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
			let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
			let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
			return Some(Color::Rgb(r, g, b));
		}
	}
	if let Some(rest) = spec.strip_prefix("rgb:") {
		let mut parts = rest.split('/');
		let r = u8::from_str_radix(parts.next()?, 16).ok()?;
		let g = u8::from_str_radix(parts.next()?, 16).ok()?;
		let b = u8::from_str_radix(parts.next()?, 16).ok()?;
		return Some(Color::Rgb(r, g, b));
	}
	None
}

pub fn xterm_palette(idx: u8) -> (u8, u8, u8) {
	const BASE: [(u8, u8, u8); 16] = [
		(24, 26, 33),
		(224, 108, 117),
		(152, 195, 121),
		(229, 192, 123),
		(97, 175, 239),
		(198, 120, 221),
		(86, 182, 194),
		(220, 224, 232),
		(92, 99, 112),
		(255, 121, 121),
		(152, 195, 121),
		(229, 192, 123),
		(97, 175, 239),
		(198, 120, 221),
		(86, 182, 194),
		(255, 255, 255),
	];
	if idx < 16 {
		BASE[idx as usize]
	} else if idx < 232 {
		let i = idx as u32 - 16;
		let r = i / 36;
		let g = (i % 36) / 6;
		let b = i % 6;
		let f = |v: u32| if v == 0 { 0 } else { 55 + v as u8 * 40 };
		(f(r), f(g), f(b))
	} else {
		let v = 8 + (idx as u16 - 232) * 10;
		(v as u8, v as u8, v as u8)
	}
}
