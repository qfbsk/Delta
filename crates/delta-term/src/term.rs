use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

const MAX_SCROLLBACK: usize = 2000;

#[derive(Clone, Copy, Debug)]
pub struct ImagePlacement {
	pub x: usize,
	pub y: usize,
	pub cols: Option<usize>,
	pub rows: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Image {
	pub id: u32,
	pub width: usize,
	pub height: usize,
	pub rgba: Vec<u8>,
	pub placement: Option<ImagePlacement>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
	Default,
	Indexed(u8),
	Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
	pub fg: Color,
	pub bg: Color,
	pub bold: bool,
	pub underline: bool,
	pub reverse: bool,
}

impl Default for Attrs {
	fn default() -> Self {
		Self {
			fg: Color::Default,
			bg: Color::Default,
			bold: false,
			underline: false,
			reverse: false,
		}
	}
}

#[derive(Clone, Copy, Debug)]
pub struct Cell {
	pub ch: char,
	pub width: u8,
	pub attrs: Attrs,
}

impl Default for Cell {
	fn default() -> Self {
		Self {
			ch: ' ',
			width: 1,
			attrs: Attrs::default(),
		}
	}
}

#[derive(Clone, Debug)]
pub struct Screen {
	pub cols: usize,
	pub rows: usize,
	pub cells: Vec<Cell>,
	pub cursor_x: usize,
	pub cursor_y: usize,
	pub scroll_top: usize,
	pub scroll_bottom: usize,
	pub wrap_pending: bool,
	pub history: VecDeque<Vec<Cell>>,
	saved_x: usize,
	saved_y: usize,
}

impl Screen {
	pub fn new(cols: usize, rows: usize) -> Self {
		let mut s = Self {
			cols,
			rows,
			cells: Vec::new(),
			cursor_x: 0,
			cursor_y: 0,
			scroll_top: 0,
			scroll_bottom: rows.saturating_sub(1),
			wrap_pending: false,
			history: VecDeque::new(),
			saved_x: 0,
			saved_y: 0,
		};
		s.clear();
		s
	}

	pub fn clear(&mut self) {
		self.cells = vec![Cell::default(); self.cols * self.rows];
		self.history.clear();
		self.cursor_x = 0;
		self.cursor_y = 0;
		self.scroll_top = 0;
		self.scroll_bottom = self.rows.saturating_sub(1);
		self.wrap_pending = false;
	}

	pub fn resize(&mut self, cols: usize, rows: usize) {
		let cols = cols.max(1);
		let rows = rows.max(1);
		if cols == self.cols && rows == self.rows {
			return;
		}
		let mut cells = vec![Cell::default(); cols * rows];
		for y in 0..rows.min(self.rows) {
			for x in 0..cols.min(self.cols) {
				cells[y * cols + x] = self.cells[y * self.cols + x];
			}
		}
		self.cols = cols;
		self.rows = rows;
		self.cells = cells;
		self.history.clear();
		self.cursor_x = self.cursor_x.min(cols - 1);
		self.cursor_y = self.cursor_y.min(rows - 1);
		self.scroll_top = 0;
		self.scroll_bottom = rows - 1;
		self.wrap_pending = false;
	}

	pub fn index(&self, x: usize, y: usize) -> usize {
		y * self.cols + x
	}

	pub fn cell(&self, x: usize, y: usize) -> Cell {
		if x >= self.cols || y >= self.rows {
			Cell::default()
		} else {
			self.cells[self.index(x, y)]
		}
	}

	fn set(&mut self, x: usize, y: usize, cell: Cell) {
		if x < self.cols && y < self.rows {
			let i = self.index(x, y);
			self.cells[i] = cell;
		}
	}

	pub fn blank_row(&mut self, y: usize) {
		for x in 0..self.cols {
			let i = self.index(x, y);
			self.cells[i] = Cell::default();
		}
	}

	pub fn put(&mut self, ch: char, attrs: Attrs) {
		if ch == '\0' {
			return;
		}
		let width = UnicodeWidthChar::width(ch).unwrap_or(1).clamp(1, 2);
		if self.wrap_pending {
			self.newline();
		}
		if width == 2 && self.cursor_x + 1 >= self.cols {
			self.newline();
		}
		let x = self.cursor_x;
		let y = self.cursor_y;
		self.set(
			x,
			y,
			Cell {
				ch,
				width: width as u8,
				attrs,
			},
		);
		if width == 2 {
			self.set(
				x + 1,
				y,
				Cell {
					ch: ' ',
					width: 0,
					attrs,
				},
			);
		}
		if x + width >= self.cols {
			self.cursor_x = self.cols - 1;
			self.wrap_pending = true;
		} else {
			self.cursor_x = x + width;
			self.wrap_pending = false;
		}
	}

	pub fn newline(&mut self) {
		self.wrap_pending = false;
		if self.cursor_y == self.scroll_bottom {
			self.scroll_up(1);
		} else if self.cursor_y + 1 < self.rows {
			self.cursor_y += 1;
		}
		self.cursor_x = 0;
	}

	pub fn carriage_return(&mut self) {
		self.cursor_x = 0;
		self.wrap_pending = false;
	}

	pub fn backspace(&mut self) {
		self.cursor_x = self.cursor_x.saturating_sub(1);
		self.wrap_pending = false;
	}

	pub fn tab(&mut self) {
		let next = (self.cursor_x / 8 + 1) * 8;
		self.cursor_x = next.min(self.cols - 1);
		self.wrap_pending = false;
	}

	pub fn scroll_up(&mut self, n: usize) {
		let top = self.scroll_top;
		let bottom = self.scroll_bottom;
		if top > bottom || bottom >= self.rows {
			return;
		}
		for _ in 0..n {
			if top == 0 && bottom + 1 == self.rows {
				self.history.push_back(self.cells[0..self.cols].to_vec());
				while self.history.len() > MAX_SCROLLBACK {
					self.history.pop_front();
				}
			}
			for y in top..bottom {
				let src = (y + 1) * self.cols;
				let dst = y * self.cols;
				for x in 0..self.cols {
					self.cells[dst + x] = self.cells[src + x];
				}
			}
			self.blank_row(bottom);
		}
	}

	pub fn scroll_down(&mut self, n: usize) {
		let top = self.scroll_top;
		let bottom = self.scroll_bottom;
		if top > bottom || bottom >= self.rows {
			return;
		}
		for _ in 0..n {
			for y in (top + 1..=bottom).rev() {
				let src = (y - 1) * self.cols;
				let dst = y * self.cols;
				for x in 0..self.cols {
					self.cells[dst + x] = self.cells[src + x];
				}
			}
			self.blank_row(top);
		}
	}

	pub fn erase_display(&mut self, mode: usize) {
		match mode {
			0 => {
				for x in self.cursor_x..self.cols {
					self.set(x, self.cursor_y, Cell::default());
				}
				for y in self.cursor_y + 1..self.rows {
					self.blank_row(y);
				}
			}
			1 => {
				for y in 0..self.cursor_y {
					self.blank_row(y);
				}
				for x in 0..=self.cursor_x.min(self.cols - 1) {
					self.set(x, self.cursor_y, Cell::default());
				}
			}
			_ => {
				for y in 0..self.rows {
					self.blank_row(y);
				}
				if mode == 3 {
					self.history.clear();
				}
			}
		}
		self.wrap_pending = false;
	}

	pub fn erase_line(&mut self, mode: usize) {
		match mode {
			0 => {
				for x in self.cursor_x..self.cols {
					self.set(x, self.cursor_y, Cell::default());
				}
			}
			1 => {
				for x in 0..=self.cursor_x.min(self.cols - 1) {
					self.set(x, self.cursor_y, Cell::default());
				}
			}
			_ => self.blank_row(self.cursor_y),
		}
		self.wrap_pending = false;
	}

	pub fn move_to(&mut self, x: usize, y: usize) {
		self.cursor_x = x.min(self.cols - 1);
		self.cursor_y = y.min(self.rows - 1);
		self.wrap_pending = false;
	}

	pub fn view_cell(&self, x: usize, y: usize, offset: usize) -> Cell {
		if offset == 0 {
			return self.cell(x, y);
		}
		let history_len = self.history.len();
		let offset = offset.min(history_len);
		let index = history_len.saturating_sub(offset) + y;
		if index < history_len {
			self.history
				.get(index)
				.and_then(|row| row.get(x))
				.copied()
				.unwrap_or_default()
		} else {
			self.cell(x, index - history_len)
		}
	}

	pub fn save_cursor(&mut self) {
		self.saved_x = self.cursor_x;
		self.saved_y = self.cursor_y;
	}

	pub fn restore_cursor(&mut self) {
		self.cursor_x = self.saved_x.min(self.cols - 1);
		self.cursor_y = self.saved_y.min(self.rows - 1);
		self.wrap_pending = false;
	}

	pub fn insert_chars(&mut self, n: usize) {
		let y = self.cursor_y;
		let x = self.cursor_x;
		let n = n.min(self.cols - x);
		for i in (x..self.cols).rev() {
			let src = if i >= x + n { i - n } else { self.cols - 1 };
			let cell = if i >= x + n {
				self.cell(src, y)
			} else {
				Cell::default()
			};
			self.set(i, y, cell);
		}
		self.wrap_pending = false;
	}

	pub fn delete_chars(&mut self, n: usize) {
		let y = self.cursor_y;
		let x = self.cursor_x;
		let n = n.min(self.cols - x);
		for i in x..self.cols {
			let cell = if i + n < self.cols {
				self.cell(i + n, y)
			} else {
				Cell::default()
			};
			self.set(i, y, cell);
		}
		self.wrap_pending = false;
	}

	pub fn erase_chars(&mut self, n: usize) {
		let y = self.cursor_y;
		for x in self.cursor_x..(self.cursor_x + n).min(self.cols) {
			self.set(x, y, Cell::default());
		}
		self.wrap_pending = false;
	}

	pub fn insert_lines(&mut self, n: usize) {
		let n = n.min(self.scroll_bottom.saturating_sub(self.cursor_y));
		for _ in 0..n {
			for y in (self.cursor_y + 1..=self.scroll_bottom).rev() {
				let src = (y - 1) * self.cols;
				let dst = y * self.cols;
				for x in 0..self.cols {
					self.cells[dst + x] = self.cells[src + x];
				}
			}
			self.blank_row(self.cursor_y);
		}
		self.wrap_pending = false;
	}

	pub fn delete_lines(&mut self, n: usize) {
		let n = n.min(self.scroll_bottom.saturating_sub(self.cursor_y));
		for _ in 0..n {
			for y in self.cursor_y..self.scroll_bottom {
				let src = (y + 1) * self.cols;
				let dst = y * self.cols;
				for x in 0..self.cols {
					self.cells[dst + x] = self.cells[src + x];
				}
			}
			self.blank_row(self.scroll_bottom);
		}
		self.wrap_pending = false;
	}
}

pub struct Terminal {
	pub main: Screen,
	pub alt: Option<Screen>,
	pub alt_active: bool,
	pub attrs: Attrs,
	pub cursor_visible: bool,
	pub app_cursor: bool,
	pub wrap_mode: bool,
	pub scroll_offset: usize,
	pub images: Vec<Image>,
	next_image_id: u32,
}

impl Terminal {
	pub fn new(cols: usize, rows: usize) -> Self {
		Self {
			main: Screen::new(cols, rows),
			alt: None,
			alt_active: false,
			attrs: Attrs::default(),
			cursor_visible: true,
			app_cursor: false,
			wrap_mode: true,
			scroll_offset: 0,
			images: Vec::new(),
			next_image_id: 1,
		}
	}

	pub fn rows(&self) -> usize {
		self.active().rows
	}

	pub fn active(&self) -> &Screen {
		if self.alt_active {
			self.alt.as_ref().unwrap_or(&self.main)
		} else {
			&self.main
		}
	}

	pub fn active_mut(&mut self) -> &mut Screen {
		if self.alt_active {
			self.alt.as_mut().unwrap_or(&mut self.main)
		} else {
			&mut self.main
		}
	}

	pub fn resize(&mut self, cols: usize, rows: usize) {
		self.main.resize(cols, rows);
		self.scroll_offset = 0;
		if let Some(alt) = self.alt.as_mut() {
			alt.resize(cols, rows);
		}
	}

	pub fn reset(&mut self) {
		self.main.clear();
		self.alt = None;
		self.alt_active = false;
		self.attrs = Attrs::default();
		self.cursor_visible = true;
		self.app_cursor = false;
		self.wrap_mode = true;
		self.scroll_offset = 0;
		self.images.clear();
		self.next_image_id = 1;
	}

	pub fn enter_alt(&mut self) {
		if self.alt_active {
			return;
		}
		let mut alt = Screen::new(self.main.cols, self.main.rows);
		alt.clear();
		self.alt = Some(alt);
		self.alt_active = true;
	}

	pub fn leave_alt(&mut self) {
		self.alt_active = false;
		self.alt = None;
	}

	pub fn set_image(
		&mut self,
		id: Option<u32>,
		width: usize,
		height: usize,
		rgba: Vec<u8>,
		placement: Option<ImagePlacement>,
	) -> u32 {
		let id = id.unwrap_or(self.next_image_id).max(1);
		self.next_image_id = self.next_image_id.max(id + 1);
		self.images.retain(|image| image.id != id);
		self.images.push(Image {
			id,
			width,
			height,
			rgba,
			placement,
		});
		id
	}

	pub fn place_image(
		&mut self,
		id: u32,
		x: usize,
		y: usize,
		cols: Option<usize>,
		rows: Option<usize>,
	) {
		if let Some(image) = self.images.iter_mut().find(|image| image.id == id) {
			image.placement = Some(ImagePlacement { x, y, cols, rows });
		}
	}

	pub fn delete_images(&mut self, id: Option<u32>) {
		match id {
			Some(id) => self.images.retain(|image| image.id != id),
			None => self.images.clear(),
		}
	}

	pub fn scroll_view(&mut self, delta: isize) {
		if self.alt_active {
			return;
		}
		let max = self.main.history.len() as isize;
		self.scroll_offset = ((self.scroll_offset as isize + delta).clamp(0, max)) as usize;
	}

	pub fn put_char(&mut self, ch: char) {
		self.scroll_offset = 0;
		if !self.wrap_mode && self.active().cursor_x + 1 >= self.active().cols {
			// Without autowrap, a full line just overwrites the last cell.
			let attrs = self.attrs;
			let x = self.active().cols.saturating_sub(1);
			let y = self.active().cursor_y;
			self.active_mut().set(
				x,
				y,
				Cell {
					ch,
					width: 1,
					attrs,
				},
			);
			return;
		}
		let attrs = self.attrs;
		self.active_mut().put(ch, attrs);
	}
}
