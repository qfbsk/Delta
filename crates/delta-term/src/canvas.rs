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
	IntersectClipRect, HALFTONE, LineTo, MoveToEx, OUT_DEFAULT_PRECIS, PS_SOLID, Rectangle,
	ReleaseDC, RestoreDC, RoundRect, SRCCOPY, SaveDC, SelectObject, SetBkMode,
	SetStretchBltMode, SetTextColor, StretchBlt, TRANSPARENT, TextOutW,
};
use windows_sys::Win32::Graphics::GdiPlus::{
	FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath,
	GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath,
	GdipDeletePen, GdipDrawPath, GdipFillPath, GdipSetSmoothingMode, GdiplusShutdown,
	GdiplusStartup, GdiplusStartupInput, GpBrush, GpGraphics, GpPath, GpSolidFill,
	SmoothingModeAntiAlias, UnitPixel,
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
	/// GDI+ 也画在这块 DC 上；圆角靠它抗锯齿，GDI 的 RoundRect 是硬像素阶梯。
	graphics: *mut GpGraphics,
	gdiplus: usize,
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
			let mut token: usize = 0;
			let mut startup: GdiplusStartupInput = zeroed();
			startup.GdiplusVersion = 1;
			GdiplusStartup(&mut token, &startup, null_mut());
			let mut graphics: *mut GpGraphics = null_mut();
			GdipCreateFromHDC(mem, &mut graphics);
			let mut canvas = Self {
				mem,
				bmp: null_mut(),
				old: null_mut(),
				graphics,
				gdiplus: token,
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
			let mut graphics: *mut GpGraphics = null_mut();
			GdipCreateFromHDC(mem, &mut graphics);
			if !self.graphics.is_null() {
				GdipDeleteGraphics(self.graphics);
			}
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
			self.graphics = graphics;
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

	/// Draws [left, top)-[right, bottom) of what is already in the frame,
	/// shrunk into `x`, `y`, `w`, `h`. The lifted-window thumbnail is the
	/// window's own picture, so it never shows stale or invented content.
	pub fn stretch_from(
		&self,
		left: i32,
		top: i32,
		right: i32,
		bottom: i32,
		x: i32,
		y: i32,
		w: i32,
		h: i32,
	) {
		let (sw, sh) = (right - left, bottom - top);
		if sw <= 0 || sh <= 0 || w <= 0 || h <= 0 {
			return;
		}
		unsafe {
			let dc = CreateCompatibleDC(self.mem);
			if dc.is_null() {
				return;
			}
			let bmp = CreateCompatibleBitmap(self.mem, sw, sh);
			if bmp.is_null() {
				DeleteDC(dc);
				return;
			}
			let old = SelectObject(dc, bmp);
			BitBlt(dc, 0, 0, sw, sh, self.mem, left, top, SRCCOPY);
			SetStretchBltMode(self.mem, HALFTONE as i32);
			StretchBlt(self.mem, x, y, w, h, dc, 0, 0, sw, sh, SRCCOPY);
			SelectObject(dc, old);
			DeleteObject(bmp);
			DeleteDC(dc);
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

	/// Rounded rectangle with only its edge painted, for pane frames, drop
	/// previews and the floating window box. `width` is the line thickness in
	/// pixels.
	pub fn round_outline(
		&self,
		left: i32,
		top: i32,
		right: i32,
		bottom: i32,
		radius: i32,
		width: i32,
		color: COLORREF,
	) {
		if right <= left || bottom <= top {
			return;
		}
		let path = round_path(self.graphics, left, top, right, bottom, radius);
		if path.is_null() {
			self.round_outline_gdi(left, top, right, bottom, radius, width, color);
			return;
		}
		unsafe {
			let mut pen = null_mut();
			if GdipCreatePen1(
				argb(color),
				width.max(1) as f32,
				UnitPixel,
				&mut pen,
			) == 0
			{
				GdipDrawPath(self.graphics, pen, path);
				GdipDeletePen(pen);
			}
			GdipDeletePath(path);
		}
	}

	fn round_outline_gdi(
		&self,
		left: i32,
		top: i32,
		right: i32,
		bottom: i32,
		radius: i32,
		width: i32,
		color: COLORREF,
	) {
		unsafe {
			let brush = windows_sys::Win32::Graphics::Gdi::GetStockObject(5); // NULL_BRUSH
			let old_brush = SelectObject(self.mem, brush);
			let pen = CreatePen(PS_SOLID, width.max(1), color);
			let old_pen = SelectObject(self.mem, pen as HGDIOBJ);
			let d = (radius * 2).max(2);
			RoundRect(self.mem, left, top, right, bottom, d, d);
			SelectObject(self.mem, old_pen);
			SelectObject(self.mem, old_brush);
			DeleteObject(pen);
		}
	}

	/// Filled rounded rectangle. `line` draws its edge; `None` leaves the edge
	/// as the fill. `radius` is the corner radius in pixels.
	pub fn round_rect(
		&self,
		left: i32,
		top: i32,
		right: i32,
		bottom: i32,
		radius: i32,
		fill: COLORREF,
		line: Option<COLORREF>,
	) {
		if right <= left || bottom <= top {
			return;
		}
		let path = round_path(self.graphics, left, top, right, bottom, radius);
		if path.is_null() {
			unsafe {
				let brush = CreateSolidBrush(fill);
				let pen = CreatePen(PS_SOLID, 1, line.unwrap_or(fill));
				let old_brush = SelectObject(self.mem, brush as HGDIOBJ);
				let old_pen = SelectObject(self.mem, pen as HGDIOBJ);
				let d = (radius * 2).max(2);
				RoundRect(self.mem, left, top, right, bottom, d, d);
				SelectObject(self.mem, old_pen);
				SelectObject(self.mem, old_brush);
				DeleteObject(pen);
				DeleteObject(brush);
			}
			return;
		}
		unsafe {
			let mut brush: *mut GpSolidFill = null_mut();
			if GdipCreateSolidFill(argb(fill), &mut brush) == 0 {
				GdipFillPath(self.graphics, brush as *mut GpBrush, path);
				GdipDeleteBrush(brush as *mut GpBrush);
			}
			if let Some(color) = line {
				let mut pen = null_mut();
				if GdipCreatePen1(argb(color), 1.0, UnitPixel, &mut pen) == 0 {
					GdipDrawPath(self.graphics, pen, path);
					GdipDeletePen(pen);
				}
			}
			GdipDeletePath(path);
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
			if !self.graphics.is_null() {
				GdipDeleteGraphics(self.graphics);
			}
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
			GdiplusShutdown(self.gdiplus);
		}
	}
}

/// Path for a rounded rectangle. Coordinates sit on pixel centres (n + 0.5)
/// so a one-pixel stroke stays one pixel wide instead of smearing over two.
/// Returns null when GDI+ is unavailable; callers fall back to GDI RoundRect.
fn round_path(
	graphics: *mut GpGraphics,
	left: i32,
	top: i32,
	right: i32,
	bottom: i32,
	radius: i32,
) -> *mut GpPath {
	if graphics.is_null() {
		return null_mut();
	}
	unsafe {
		GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
		let mut path: *mut GpPath = null_mut();
		if GdipCreatePath(FillModeAlternate, &mut path) != 0 {
			return null_mut();
		}
		let half_w = (right - left).max(2) as f32 / 2.0;
		let half_h = (bottom - top).max(2) as f32 / 2.0;
		let r = (radius.max(0) as f32).min(half_w).min(half_h);
		let d = r * 2.0;
		// GDI+ 把像素中心放在整数坐标上，直线落在整数上才是实心一像素。
		let l = left as f32;
		let t = top as f32;
		let rt = (right - 1) as f32;
		let b = (bottom - 1) as f32;
		GdipAddPathArc(path, l, t, d, d, 180.0, 90.0);
		GdipAddPathArc(path, rt - d, t, d, d, 270.0, 90.0);
		GdipAddPathArc(path, rt - d, b - d, d, d, 0.0, 90.0);
		GdipAddPathArc(path, l, b - d, d, d, 90.0, 90.0);
		GdipClosePathFigure(path);
		path
	}
}

/// COLORREF (0x00BBGGRR) as the ARGB GDI+ wants.
fn argb(color: COLORREF) -> u32 {
	0xFF00_0000 | ((color & 0xFF) << 16) | (color & 0xFF00) | ((color >> 16) & 0xFF)
}

fn pick_font() -> &'static str {
	if font_exists("JetBrainsMono NFM") {
		"JetBrainsMono NFM"
	} else if font_exists("Cascadia Mono") {
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

pub fn rgb_tuple(c: (u8, u8, u8)) -> COLORREF {
	rgb(c.0, c.1, c.2)
}

// Catppuccin Mocha, the theme named in the reference ghostty config
// (`theme = Catppuccin Mocha`). Base is the terminal background, mantle the
// bars, surface the chips, and the bright accents the badges.
pub const BASE: (u8, u8, u8) = (30, 30, 46); // #1e1e2e
pub const MANTLE: (u8, u8, u8) = (24, 24, 37); // #181825
pub const SURFACE0: (u8, u8, u8) = (49, 50, 68); // #313244
pub const SURFACE1: (u8, u8, u8) = (69, 71, 90); // #45475a
pub const OVERLAY1: (u8, u8, u8) = (127, 132, 156); // #7f849c
pub const SUBTEXT0: (u8, u8, u8) = (166, 173, 200); // #a6adc8
pub const TEXT: (u8, u8, u8) = (205, 214, 244); // #cdd6f4
pub const BLUE: (u8, u8, u8) = (137, 180, 250); // #89b4fa
pub const GREEN: (u8, u8, u8) = (166, 227, 161); // #a6e3a1
pub const YELLOW: (u8, u8, u8) = (249, 226, 175); // #f9e2af
pub const PEACH: (u8, u8, u8) = (250, 179, 135); // #fab387
pub const PINK: (u8, u8, u8) = (245, 194, 231); // #f5c2e7
pub const MAUVE: (u8, u8, u8) = (203, 166, 247); // #cba6f7
pub const TEAL: (u8, u8, u8) = (148, 226, 213); // #94e2d5

pub const DEFAULT_FG: (u8, u8, u8) = TEXT;
pub const DEFAULT_BG: (u8, u8, u8) = BASE;
pub const SELECTION_BG: (u8, u8, u8) = SURFACE1;
pub const BORDER: (u8, u8, u8) = SURFACE0;
pub const BORDER_ACTIVE: (u8, u8, u8) = BLUE;
pub const STATUS_BG: (u8, u8, u8) = MANTLE;
pub const STATUS_FG: (u8, u8, u8) = TEXT;
pub const ACCENT: (u8, u8, u8) = BLUE;

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
	// Catppuccin Mocha's ANSI sixteen, so program output sits on the same
	// palette as the chrome.
	const PALETTE: [(u8, u8, u8); 16] = [
		(69, 71, 90),    // black: surface1
		(243, 139, 168), // red
		(166, 227, 161), // green
		(249, 226, 175), // yellow
		(137, 180, 250), // blue
		(245, 194, 231), // magenta: pink
		(148, 226, 213), // cyan: teal
		(186, 194, 222), // white: subtext1
		(88, 91, 112),   // bright black: surface2
		(243, 139, 168), // bright red
		(166, 227, 161), // bright green
		(249, 226, 175), // bright yellow
		(137, 180, 250), // bright blue
		(245, 194, 231), // bright magenta
		(148, 226, 213), // bright cyan
		(166, 173, 200), // bright white: subtext0
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
