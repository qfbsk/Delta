//! GDI double-buffered cell canvas. The setup follows the first probe at
//! b0c34b7 (`win.rs::init_gdi`) and Windows Terminal's GDI fallback shape,
//! but everything is drawn from the delta-vt grid each frame.

use std::collections::HashMap;
use std::mem::zeroed;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::SIZE;
use windows_sys::Win32::Foundation::{COLORREF, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
	BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap, CreateCompatibleDC,
	CreateFontW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET, DeleteDC, DeleteObject, FF_MODERN,
	FIXED_PITCH, FW_NORMAL, FillRect, GetDC, GetTextExtentPoint32W, HDC, HFONT, HGDIOBJ,
	IntersectClipRect, LineTo, MoveToEx, OUT_DEFAULT_PRECIS, PS_SOLID, Rectangle, ReleaseDC,
	RestoreDC, SRCCOPY, SaveDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};

/// The size every pane starts at; status bar, borders and modals always use it.
pub const FONT_SIZE: i32 = 16;
/// Ctrl+= / Ctrl+- stop here. Below 8 the glyphs collapse; above 96 one cell is
/// wider than a pane on a normal screen.
pub const MIN_FONT_SIZE: i32 = 8;
pub const MAX_FONT_SIZE: i32 = 96;

#[derive(Clone, Copy)]
struct Face {
	font: HFONT,
	cell_w: i32,
	cell_h: i32,
}
pub struct Canvas {
	pub mem: HDC,
	bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
	old: HGDIOBJ,
	face: &'static str,
	faces: HashMap<i32, Face>,
	/// Font size currently selected; `cell_w`/`cell_h` belong to it.
	pub font_size: i32,
	pub cell_w: i32,
	pub cell_h: i32,
	pub width: i32,
	pub height: i32,
	pub bg: COLORREF,
}

impl Canvas {
	pub fn new(hwnd: HWND) -> Option<Self> {
		unsafe {
			let screen = GetDC(hwnd);
			if screen.is_null() {
				return None;
			}
			let mem = CreateCompatibleDC(screen);
			if mem.is_null() {
				ReleaseDC(hwnd, screen);
				return None;
			}
			ReleaseDC(hwnd, screen);
			let mut canvas = Self {
				mem,
				bmp: null_mut(),
				old: null_mut(),
				face: pick_font(),
				faces: HashMap::new(),
				font_size: FONT_SIZE,
				cell_w: 8,
				cell_h: 16,
				width: 0,
				height: 0,
				bg: 0,
			};
			let face = canvas.face_for(FONT_SIZE);
			SelectObject(mem, face.font as HGDIOBJ);
			SetBkMode(mem, TRANSPARENT as i32);
			canvas.cell_w = face.cell_w;
			canvas.cell_h = face.cell_h;
			Some(canvas)
		}
	}

	/// The font for `size`, created and measured on the memory DC on first use.
	fn face_for(&mut self, size: i32) -> Face {
		if let Some(face) = self.faces.get(&size) {
			return *face;
		}
		unsafe {
			let name = wide(self.face);
			let font = CreateFontW(
				-size,
				0,
				0,
				0,
				FW_NORMAL as i32,
				0,
				0,
				0,
				DEFAULT_CHARSET as u32,
				OUT_DEFAULT_PRECIS as u32,
				CLIP_DEFAULT_PRECIS as u32,
				CLEARTYPE_QUALITY as u32,
				(FIXED_PITCH | FF_MODERN) as u32,
				name.as_ptr(),
			);
			let old = SelectObject(self.mem, font as HGDIOBJ);
			let mut measured: SIZE = zeroed();
			let m = wide("M");
			let ok = GetTextExtentPoint32W(self.mem, m.as_ptr(), 1, &mut measured);
			SelectObject(self.mem, old);
			let cell_w = if ok != 0 && measured.cx > 0 {
				measured.cx
			} else {
				8
			};
			let cell_h = if ok != 0 && measured.cy > 0 {
				measured.cy
			} else {
				size.max(1)
			};
			let face = Face {
				font,
				cell_w,
				cell_h,
			};
			self.faces.insert(size, face);
			face
		}
	}

	/// Cell size for `size`, creating the font if this is the first use.
	pub fn metrics(&mut self, size: i32) -> (i32, i32) {
		let face = self.face_for(size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE));
		(face.cell_w, face.cell_h)
	}

	/// Draw from here on with the font for `size`; cell sizes follow it.
	pub fn select_font(&mut self, size: i32) {
		let size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
		let face = self.face_for(size);
		unsafe {
			SelectObject(self.mem, face.font as HGDIOBJ);
		}
		self.font_size = size;
		self.cell_w = face.cell_w;
		self.cell_h = face.cell_h;
	}

	pub fn resize(&mut self, hwnd: HWND, width: i32, height: i32) {
		if width <= 0 || height <= 0 || (width == self.width && height == self.height) {
			return;
		}
		unsafe {
			let screen = GetDC(hwnd);
			if screen.is_null() {
				return;
			}
			let mem = CreateCompatibleDC(screen);
			let bmp = CreateCompatibleBitmap(screen, width, height);
			let old = SelectObject(mem, bmp);
			if let Some(face) = self.faces.get(&self.font_size) {
				SelectObject(mem, face.font as HGDIOBJ);
			}
			SetBkMode(mem, TRANSPARENT as i32);
			ReleaseDC(hwnd, screen);
			if !self.bmp.is_null() {
				SelectObject(self.mem, self.old);
				DeleteObject(self.bmp);
			}
			if !self.mem.is_null() {
				DeleteDC(self.mem);
			}
			self.mem = mem;
			self.bmp = bmp;
			self.old = old;
			self.width = width;
			self.height = height;
		}
	}

	pub fn clear(&self) {
		unsafe {
			let rect = RECT {
				left: 0,
				top: 0,
				right: self.width,
				bottom: self.height,
			};
			let brush = CreateSolidBrush(self.bg);
			FillRect(self.mem, &rect, brush);
			DeleteObject(brush);
		}
	}

	pub fn fill(&self, left: i32, top: i32, right: i32, bottom: i32, color: COLORREF) {
		if right <= left || bottom <= top {
			return;
		}
		unsafe {
			let rect = RECT {
				left,
				top,
				right,
				bottom,
			};
			let brush = CreateSolidBrush(color);
			FillRect(self.mem, &rect, brush);
			DeleteObject(brush);
		}
	}

	pub fn clip(&self, rect: (i32, i32, i32, i32)) -> i32 {
		unsafe {
			let saved = SaveDC(self.mem);
			IntersectClipRect(self.mem, rect.0, rect.1, rect.2, rect.3);
			saved
		}
	}

	pub fn unclip(&self, saved: i32) {
		unsafe {
			RestoreDC(self.mem, saved);
		}
	}

	pub fn text(&self, x: i32, y: i32, text: &str, color: COLORREF) {
		if text.is_empty() {
			return;
		}
		let units = wide(text);
		unsafe {
			SetTextColor(self.mem, color);
			TextOutW(self.mem, x, y, units.as_ptr(), (units.len() - 1) as i32);
		}
	}

	pub fn line(&self, x1: i32, y1: i32, x2: i32, y2: i32, color: COLORREF) {
		unsafe {
			let pen = CreatePen(PS_SOLID, 1, color);
			let old = SelectObject(self.mem, pen as HGDIOBJ);
			MoveToEx(self.mem, x1, y1, null_mut());
			LineTo(self.mem, x2, y2);
			SelectObject(self.mem, old);
			DeleteObject(pen);
		}
	}

	pub fn outline(&self, left: i32, top: i32, right: i32, bottom: i32, color: COLORREF) {
		unsafe {
			let brush = windows_sys::Win32::Graphics::Gdi::GetStockObject(5); // NULL_BRUSH
			let old = SelectObject(self.mem, brush);
			let pen = CreatePen(PS_SOLID, 1, color);
			let old_pen = SelectObject(self.mem, pen as HGDIOBJ);
			Rectangle(self.mem, left, top, right, bottom);
			SelectObject(self.mem, old_pen);
			SelectObject(self.mem, old);
			DeleteObject(pen);
		}
	}

	pub fn present(&self, hwnd: HWND) {
		unsafe {
			let screen = GetDC(hwnd);
			if !screen.is_null() {
				BitBlt(
					screen,
					0,
					0,
					self.width,
					self.height,
					self.mem,
					0,
					0,
					SRCCOPY,
				);
				ReleaseDC(hwnd, screen);
			}
		}
	}
}

impl Drop for Canvas {
	fn drop(&mut self) {
		unsafe {
			if !self.bmp.is_null() {
				SelectObject(self.mem, self.old);
				DeleteObject(self.bmp);
			}
			for (_, face) in self.faces.drain() {
				if !face.font.is_null() {
					DeleteObject(face.font);
				}
			}
			if !self.mem.is_null() {
				DeleteDC(self.mem);
			}
		}
	}
}

fn pick_font() -> &'static str {
	if font_exists("Cascadia Mono") {
		"Cascadia Mono"
	} else if font_exists("Cascadia Code") {
		"Cascadia Code"
	} else {
		"Consolas"
	}
}

fn font_exists(name: &str) -> bool {
	use windows_sys::Win32::Graphics::Gdi::EnumFontFamiliesExW;
	use windows_sys::Win32::Graphics::Gdi::LOGFONTW;
	use windows_sys::Win32::Graphics::Gdi::TEXTMETRICW;
	unsafe extern "system" fn cb(
		_lf: *const LOGFONTW,
		_tm: *const TEXTMETRICW,
		_font_type: u32,
		lparam: windows_sys::Win32::Foundation::LPARAM,
	) -> i32 {
		let flag = lparam as *mut bool;
		*flag = true;
		0
	}
	unsafe {
		let dc = GetDC(null_mut());
		if dc.is_null() {
			return false;
		}
		let mut lf: LOGFONTW = zeroed();
		let wide_name = wide(name);
		for (i, unit) in wide_name.iter().take(32).enumerate() {
			lf.lfFaceName[i] = *unit;
		}
		let mut found = false;
		EnumFontFamiliesExW(dc, &lf, Some(cb), &mut found as *mut bool as isize, 0);
		ReleaseDC(null_mut(), dc);
		found
	}
}

pub fn wide(text: &str) -> Vec<u16> {
	text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
	(r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

pub const DEFAULT_FG: (u8, u8, u8) = (212, 216, 226);
pub const DEFAULT_BG: (u8, u8, u8) = (16, 18, 24);
pub const SELECTION_BG: (u8, u8, u8) = (46, 64, 92);
pub const BORDER: (u8, u8, u8) = (40, 42, 52);
pub const BORDER_ACTIVE: (u8, u8, u8) = (96, 132, 168);
pub const STATUS_BG: (u8, u8, u8) = (24, 26, 34);
pub const STATUS_FG: (u8, u8, u8) = (160, 166, 180);
pub const ACCENT: (u8, u8, u8) = (120, 168, 220);

pub fn colorref(c: delta_vt::Color, bold: bool, background: bool) -> COLORREF {
	use delta_vt::Color;
	let (r, g, b) = match c {
		Color::Default => {
			if background {
				DEFAULT_BG
			} else {
				DEFAULT_FG
			}
		}
		Color::Indexed(n) => indexed(n, bold),
		Color::Rgb(r, g, b) => (r, g, b),
	};
	if bold && !background {
		rgb(
			r.saturating_add(24),
			g.saturating_add(24),
			b.saturating_add(24),
		)
	} else {
		rgb(r, g, b)
	}
}

fn indexed(n: u8, bold: bool) -> (u8, u8, u8) {
	const PALETTE: [(u8, u8, u8); 16] = [
		(24, 26, 32),
		(205, 92, 100),
		(120, 180, 120),
		(200, 180, 110),
		(100, 140, 200),
		(170, 120, 190),
		(100, 180, 190),
		(200, 200, 200),
		(120, 124, 136),
		(235, 120, 128),
		(150, 210, 150),
		(235, 215, 140),
		(130, 170, 235),
		(205, 150, 225),
		(130, 215, 225),
		(240, 240, 240),
	];
	if n < 16 {
		let c = PALETTE[n as usize];
		if bold && n < 8 {
			(PALETTE[(n + 8) as usize])
		} else {
			c
		}
	} else if n < 232 {
		let i = n - 16;
		let r = i / 36;
		let g = (i % 36) / 6;
		let b = i % 6;
		let f = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
		(f(r), f(g), f(b))
	} else {
		let v = 8 + (n - 232) * 10;
		(v, v, v)
	}
}
