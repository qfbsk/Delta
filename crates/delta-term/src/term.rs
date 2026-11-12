use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use delta_vt::{Color, Screen};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::canvas::{self, Canvas};
use crate::layout::Rect;
use crate::pty::ConPty;

pub const WM_APP_DATA: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 10;
pub const WM_APP_EXIT: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
	pub row: usize,
	pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
	pub anchor: Point,
	pub cursor: Point,
	pub alt: bool,
}

impl Selection {
	fn range(&self) -> (Point, Point) {
		if (self.anchor.row, self.anchor.col) <= (self.cursor.row, self.cursor.col) {
			(self.anchor, self.cursor)
		} else {
			(self.cursor, self.anchor)
		}
	}

	fn contains(&self, row: usize, col: usize) -> bool {
		let (a, b) = self.range();
		(row, col) >= (a.row, a.col) && (row, col) <= (b.row, b.col)
	}
}

pub struct TerminalPane {
	pub pty: Option<ConPty>,
	pub screen: Screen,
	queue: Arc<Mutex<Vec<u8>>>,
	/// set while a WM_APP_DATA notification is pending; cleared when the queue is drained
	data_posted: Arc<AtomicBool>,
	pub title: String,
	/// lines scrolled up from the bottom; 0 means live.
	pub scroll: usize,
	pub selection: Option<Selection>,
	pub exited: bool,
	pub bell: bool,
	pub alt: bool,
}

impl TerminalPane {
	pub fn new(
		cols: usize,
		rows: usize,
		command: &str,
		envs: &[(String, String)],
	) -> std::io::Result<Self> {
		let pty = ConPty::spawn_with_env(cols, rows, command, envs)?;
		Ok(Self {
			pty: Some(pty),
			screen: Screen::new(cols, rows),
			queue: Arc::new(Mutex::new(Vec::new())),
			data_posted: Arc::new(AtomicBool::new(false)),
			title: String::new(),
			scroll: 0,
			selection: None,
			exited: false,
			bell: false,
			alt: false,
		})
	}

	pub fn start_reader(&mut self, hwnd: HWND, pane: u32) {
		let Some(pty) = &self.pty else { return };
		let handle = pty.output_read() as isize;
		let hwnd = hwnd as isize;
		let queue = self.queue.clone();
		let data_posted = self.data_posted.clone();
		thread::spawn(move || {
			let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
			let handle = handle as windows_sys::Win32::Foundation::HANDLE;
			let mut buf = [0u8; 8192];
			loop {
				let mut n = 0u32;
				let ok = unsafe {
					windows_sys::Win32::Storage::FileSystem::ReadFile(
						handle,
						buf.as_mut_ptr(),
						buf.len() as u32,
						&mut n,
						std::ptr::null_mut(),
					)
				};
				if ok == 0 || n == 0 {
					unsafe {
						windows_sys::Win32::Foundation::CloseHandle(handle);
						PostMessageW(hwnd, WM_APP_EXIT, pane as usize, 0);
					}
					break;
				}
				if let Ok(mut q) = queue.lock() {
					if q.len() < (1 << 24) {
						q.extend_from_slice(&buf[..n as usize]);
					}
				}
				// One notification is enough no matter how many chunks arrive; the UI drains the
				// whole queue per notification. A rapid stream would otherwise bury input keys
				// under thousands of duplicate posted messages.
				if !data_posted.swap(true, Ordering::SeqCst) {
					unsafe {
						PostMessageW(hwnd, WM_APP_DATA, pane as usize, 0);
					}
				}
			}
		});
	}

	pub fn process_exited(&self) -> bool {
		match &self.pty {
			Some(pty) => pty.exited(),
			None => true,
		}
	}

	pub fn drain(&mut self) -> bool {
		self.data_posted.store(false, Ordering::SeqCst);
		let bytes = {
			let Ok(mut q) = self.queue.lock() else { return false };
			if q.is_empty() {
				return false;
			}
			std::mem::take(&mut *q)
		};
		self.screen.feed(&bytes);
		if self.screen.bell {
			self.screen.bell = false;
			self.bell = true;
		}
		self.alt = self.screen.is_alt();
		let response = self.screen.take_response();
		if !response.is_empty() {
			self.write(&response);
		}
		if let Some(cwd) = self.screen.cwd.clone() {
			let _ = cwd;
		}
		if self.screen.title != self.title {
			self.title = self.screen.title.clone();
		}
		self.scroll = self.scroll.min(self.max_scroll());
		true
	}

	pub fn write(&self, bytes: &[u8]) {
		if let Some(pty) = &self.pty {
			pty.write(bytes);
		}
	}

	pub fn max_scroll(&self) -> usize {
		if self.alt {
			0
		} else {
			self.screen.scrollback_len()
		}
	}

	pub fn scroll_up(&mut self, lines: usize) {
		self.scroll = (self.scroll + lines).min(self.max_scroll());
	}

	pub fn scroll_down(&mut self, lines: usize) {
		self.scroll = self.scroll.saturating_sub(lines);
	}

	pub fn resize(&mut self, cols: usize, rows: usize) {
		if cols == self.screen.cols && rows == self.screen.rows {
			return;
		}
		self.screen.resize(cols.max(1), rows.max(1));
		if let Some(pty) = &self.pty {
			pty.resize(cols.max(1), rows.max(1));
		}
		self.scroll = self.scroll.min(self.max_scroll());
	}

	pub fn paste(&self, text: &str) {
		let mut out = String::new();
		if self.screen.modes.bracketed_paste {
			out.push_str("\x1b[200~");
		}
		let mut first = true;
		for part in text.split('\n') {
			if !first {
				out.push('\r');
			}
			first = false;
			out.push_str(part.strip_suffix('\r').unwrap_or(part));
		}
		if self.screen.modes.bracketed_paste {
			out.push_str("\x1b[201~");
		}
		self.write(out.as_bytes());
	}

	pub fn snapshot(&self) -> String {
		if self.alt {
			self.screen.alt_text()
		} else {
			self.screen.all_text()
		}
	}

	pub fn selection_text(&self) -> Option<String> {
		let sel = self.selection?;
		if sel.alt != self.alt {
			return None;
		}
		let (a, b) = sel.range();
		if sel.alt {
			let mut out = String::new();
			for r in a.row..=b.row {
				let Some(row) = self.screen.alt_row(r) else { continue };
				let from = if r == a.row { a.col.min(row.cells.len()) } else { 0 };
				let to = if r == b.row { (b.col + 1).min(row.cells.len()) } else { row.cells.len() };
				for c in &row.cells[from..to] {
					if c.wide == 2 {
						continue;
					}
					out.push(c.ch);
					if c.has_comb {
						out.push(c.comb);
					}
				}
				if r != b.row {
					out.push('\n');
				}
			}
			Some(out)
		} else {
			Some(self.screen.text_between(a.row, a.col, b.row, b.col))
		}
	}

	pub fn cell_at_view(&self, row: usize, col: usize) -> usize {
		let top = self.view_top();
		top + row
	}

	pub fn view_top(&self) -> usize {
		if self.alt {
			0
		} else {
			let total = self.screen.scrollback_len() + self.screen.rows;
			total.saturating_sub(self.screen.rows + self.scroll)
		}
	}

	pub fn row_at(&self, idx: usize) -> Option<&delta_vt::Row> {
		if self.alt {
			self.screen.alt_row(idx)
		} else {
			self.screen.row_abs(idx)
		}
	}

	pub fn render(
		&self,
		canvas: &Canvas,
		area: Rect,
		focused: bool,
		show_cursor: bool,
		pane_number: Option<u32>,
	) {
		let cw = canvas.cell_w;
		let ch = canvas.cell_h;
		let cols = (area.width() / cw).max(1) as usize;
		let rows = (area.height() / ch).max(1) as usize;
		let (width, height) = (cols as i32 * cw, rows as i32 * ch);
		let default_bg = canvas::rgb(canvas::DEFAULT_BG.0, canvas::DEFAULT_BG.1, canvas::DEFAULT_BG.2);
		canvas.fill(area.left, area.top, area.left + width, area.top + height, default_bg);
		let saved = canvas.clip((area.left, area.top, area.left + width, area.top + height));
		let (cc, cr) = self.screen.cursor();
		let cursor_abs = if self.alt { cr } else { self.screen.scrollback_len() + cr };
		let sel = self
			.selection
			.filter(|s| s.alt == self.alt && (s.anchor.row, s.anchor.col) != (s.cursor.row, s.cursor.col));

		for row in 0..rows {
			let abs = self.view_top() + row;
			let Some(line) = self.row_at(abs) else { continue };
			let y = area.top + row as i32 * ch;
			for col in 0..cols {
				let Some(cell) = line.cells.get(col) else { continue };
				let mut fg = cell.fg;
				let mut bg = cell.bg;
				let bold = cell.attrs & delta_vt::ATTR_BOLD != 0;
				let dim = cell.attrs & delta_vt::ATTR_DIM != 0;
				let reverse = cell.attrs & delta_vt::ATTR_REVERSE != 0
					|| self.screen.modes.reverse_video;
				if reverse {
					std::mem::swap(&mut fg, &mut bg);
					if fg == Color::Default {
						fg = self.screen.default_bg;
					}
					if bg == Color::Default {
						bg = self.screen.default_fg;
					}
				}
				let mut bg_color = canvas::colorref(bg, false, true);
				let mut fg_color = canvas::colorref(fg, bold, false);
				if dim {
					fg_color = canvas::rgb(
						(((fg_color & 0xff) as u8) as u16 * 2 / 3) as u8,
						((((fg_color >> 8) & 0xff) as u8) as u16 * 2 / 3) as u8,
						((((fg_color >> 16) & 0xff) as u8) as u16 * 2 / 3) as u8,
					);
				}
				if cell.bg != Color::Default || reverse {
					canvas.fill(
						area.left + col as i32 * cw,
						y,
						area.left + (col as i32 + 1) * cw,
						y + ch,
						bg_color,
					);
				}
				if cell.wide == 2 {
					continue;
				}
				if let Some(sel) = sel {
					if sel.contains(abs, col) {
						bg_color = canvas::rgb(
							canvas::SELECTION_BG.0,
							canvas::SELECTION_BG.1,
							canvas::SELECTION_BG.2,
						);
						canvas.fill(
							area.left + col as i32 * cw,
							y,
							area.left + (col as i32 + 1) * cw * if cell.wide == 1 { 2 } else { 1 },
							y + ch,
							bg_color,
						);
					}
				}
				if show_cursor && focused && !self.exited && abs == cursor_abs && col == cc {
					canvas.fill(
						area.left + col as i32 * cw,
						y,
						area.left + (col as i32 + 1) * cw,
						y + ch,
						canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
					);
					fg_color = canvas::rgb(10, 12, 16);
				}
				if cell.ch != ' ' || cell.has_comb {
					let mut text = String::new();
					text.push(cell.ch);
					if cell.has_comb {
						text.push(cell.comb);
					}
					canvas.text(area.left + col as i32 * cw, y, &text, fg_color);
				}
				if cell.attrs & (delta_vt::ATTR_UNDERLINE | delta_vt::ATTR_DOUBLE_UNDERLINE) != 0 {
					canvas.line(
						area.left + col as i32 * cw,
						y + ch - 2,
						area.left + (col as i32 + 1) * cw,
						y + ch - 2,
						fg_color,
					);
				}
				if cell.attrs & delta_vt::ATTR_STRIKE != 0 {
					canvas.line(
						area.left + col as i32 * cw,
						y + ch / 2,
						area.left + (col as i32 + 1) * cw,
						y + ch / 2,
						fg_color,
					);
				}
			}
		}
		if let Some(n) = pane_number {
			let label = format!(" {n} ");
			let w = label.len() as i32 * cw;
			canvas.fill(area.left, area.top, area.left + w, area.top + ch, canvas::rgb(60, 70, 90));
			canvas.text(area.left, area.top, &label, canvas::rgb(240, 240, 240));
		}
		canvas.unclip(saved);
	}
}
