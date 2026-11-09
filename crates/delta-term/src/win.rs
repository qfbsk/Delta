use std::ffi::c_void;
use std::io;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;
use windows_sys::Win32::Foundation::{
	COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
	BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, CLEARTYPE_QUALITY,
	CLIP_DEFAULT_PRECIS, COLOR_WINDOW, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
	CreateSolidBrush, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC, DeleteObject, EndPaint, FF_MODERN,
	FIXED_PITCH, FW_NORMAL, FillRect, GetDC, GetTextExtentPoint32W, HBITMAP, HBRUSH, HDC, HFONT,
	HGDIOBJ, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, ReleaseDC, SRCCOPY, ScreenToClient,
	SelectObject, SetBkMode, SetTextColor, StretchDIBits, TRANSPARENT, TextOutW, UpdateWindow,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::UI::Controls::Dialogs::{
	GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
	OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::Ime::{
	CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR, HIMC,
	ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow,
	ImmSetCompositionWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
	AdjustWindowRect, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW,
	DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW,
	GetWindowLongPtrW, IDC_ARROW, LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
	SPI_GETWORKAREA, SW_SHOW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
	SystemParametersInfoW, TranslateMessage, WM_APP, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND,
	WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_LBUTTONDOWN,
	WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT, WM_SIZE, WM_SYSCHAR, WM_SYSKEYDOWN, WNDCLASSW,
	WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::even::{GroupAction, GroupPane, RecordKind};
use crate::keys::*;
use crate::layout::Dir;
use crate::opencode::Server;
use crate::pane::{Pane, PaneKind};
use crate::record::{Event, NullRecorder, Recorder};
use crate::solid::Editor;
use crate::tab::Tab;
use crate::term::{Color, Image};
use crate::theme;
use crate::vt::Parser;

const WM_APP_REFRESH: u32 = WM_APP + 1;
const WM_APP_PANE_EXIT: u32 = WM_APP + 2;
const WM_APP_TICK: u32 = WM_APP + 3;
const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveKind {
	Terminal,
	Editor,
	Group,
}

pub struct App {
	hwnd: HWND,
	tabs: Vec<Tab>,
	active_tab: usize,
	next_pane_id: u64,
	command: String,
	label: String,
	image_path: Option<String>,
	server: Option<Server>,
	recorder: Box<dyn Recorder>,
	mem_dc: HDC,
	mem_bmp: HBITMAP,
	old_bmp: HGDIOBJ,
	font: HFONT,
	default_brush: HBRUSH,
	cell_w: i32,
	cell_h: i32,
	high_surrogate: Option<u16>,
}

impl App {
	fn new(shell: Option<String>, image_path: Option<String>) -> io::Result<Self> {
		let command = command_for(shell.clone());
		let label = shell_label(shell.as_deref());
		let pane = Pane::new(1, &command, 80, 24)?;
		let recorder: Box<dyn Recorder> = Box::new(NullRecorder);
		recorder.record(Event::WindowOpened {
			window: 1,
			kind: "terminal",
			label: label.clone(),
		});
		recorder.record(Event::CommandStarted {
			window: 1,
			command: command.clone(),
		});
		Ok(Self {
			hwnd: null_mut(),
			tabs: vec![Tab::new(pane, command.clone(), label.clone())],
			active_tab: 0,
			next_pane_id: 2,
			command,
			label,
			image_path,
			server: None,
			recorder,
			mem_dc: null_mut(),
			mem_bmp: null_mut(),
			old_bmp: null_mut(),
			font: null_mut(),
			default_brush: null_mut(),
			cell_w: 0,
			cell_h: 0,
			high_surrogate: None,
		})
	}

	fn record(&self, event: Event) {
		self.recorder.record(event);
	}

	fn invalidate(&self) {
		if !self.hwnd.is_null() {
			unsafe {
				InvalidateRect(self.hwnd, null(), 0);
			}
		}
	}

	fn active_tab(&self) -> Option<&Tab> {
		self.tabs.get(self.active_tab)
	}

	fn active_tab_mut(&mut self) -> Option<&mut Tab> {
		self.tabs.get_mut(self.active_tab)
	}

	fn active_pane(&self) -> Option<&Pane> {
		self.active_tab().and_then(|tab| tab.active_pane())
	}

	fn active_kind(&self) -> Option<ActiveKind> {
		match &self.active_pane()?.kind {
			PaneKind::Terminal { .. } => Some(ActiveKind::Terminal),
			PaneKind::Editor(_) => Some(ActiveKind::Editor),
			PaneKind::Group(_) => Some(ActiveKind::Group),
		}
	}

	fn active_editor_mut(&mut self) -> Option<&mut Editor> {
		self.active_tab_mut()?.active_pane_mut()?.editor_mut()
	}

	fn active_group_mut(&mut self) -> Option<&mut GroupPane> {
		match &mut self.active_tab_mut()?.active_pane_mut()?.kind {
			PaneKind::Group(group) => Some(group),
			_ => None,
		}
	}

	fn active_rect(&self) -> Option<RECT> {
		let id = self.active_tab().and_then(|tab| tab.active)?;
		self.rects()
			.into_iter()
			.find(|(pane, _)| *pane == id)
			.map(|(_, rect)| rect)
	}

	fn tab_bar_height(&self) -> i32 {
		if self.tabs.is_empty() {
			0
		} else {
			self.cell_h + 8
		}
	}

	fn content_rect(&self, client: RECT) -> RECT {
		RECT {
			left: client.left,
			top: client.top + self.tab_bar_height(),
			right: client.right,
			bottom: client.bottom,
		}
	}

	fn tab_rects(&self, client: RECT) -> Vec<(usize, RECT)> {
		let height = self.tab_bar_height();
		let width = 180;
		let mut out = Vec::new();
		for i in 0..self.tabs.len() {
			let left = client.left + i as i32 * width;
			if left >= client.right {
				break;
			}
			out.push((
				i,
				RECT {
					left,
					top: client.top,
					right: (left + width).min(client.right),
					bottom: client.top + height,
				},
			));
		}
		out
	}

	fn cols_for(&self, rect: RECT) -> usize {
		(((rect.right - rect.left - 2).max(1) / self.cell_w.max(1)).max(1)) as usize
	}

	fn rows_for(&self, rect: RECT) -> usize {
		(((rect.bottom - rect.top - 2).max(1) / self.cell_h.max(1)).max(1)) as usize
	}

	fn start_pane_reader(&mut self, id: u64) {
		let (out, inp, proc, term) = match self
			.tabs
			.iter()
			.flat_map(|tab| tab.panes.iter())
			.find(|pane| pane.id == id)
		{
			Some(pane) => match pane.terminal_handles() {
				Some(handles) => handles,
				None => return,
			},
			None => return,
		};
		let hwnd = self.hwnd as usize;
		let dump_path = std::env::var("DELTA_DUMP").ok();
		let handle = thread::spawn(move || {
			use std::io::Write;
			let mut dump = dump_path.and_then(|path| {
				std::fs::OpenOptions::new()
					.create(true)
					.append(true)
					.open(path)
					.ok()
			});
			let out = out as HANDLE;
			let inp = inp as HANDLE;
			let mut parser = Parser::new();
			let mut buf = [0u8; 16384];
			loop {
				let mut read = 0u32;
				let ok = unsafe {
					ReadFile(
						out,
						buf.as_mut_ptr(),
						buf.len() as u32,
						&mut read,
						null_mut(),
					)
				};
				if ok == 0 || read == 0 {
					break;
				}
				if let Some(file) = dump.as_mut() {
					let _ = file.write_all(&buf[..read as usize]);
				}
				let mut responses = Vec::new();
				if let Ok(mut term) = term.lock() {
					parser.feed(&buf[..read as usize], &mut term, &mut responses);
				}
				if !responses.is_empty() {
					let mut written = 0u32;
					unsafe {
						WriteFile(
							inp,
							responses.as_ptr(),
							responses.len() as u32,
							&mut written,
							null_mut(),
						);
					}
				}
				unsafe {
					PostMessageW(hwnd as HWND, WM_APP_REFRESH, 0, 0);
				}
			}
			unsafe {
				PostMessageW(hwnd as HWND, WM_APP_PANE_EXIT, id as usize, 0);
			}
		});
		if let Some(pane) = self
			.tabs
			.iter_mut()
			.flat_map(|tab| tab.panes.iter_mut())
			.find(|pane| pane.id == id)
		{
			pane.set_reader(handle);
		}

		thread::spawn(move || unsafe {
			WaitForSingleObject(proc as HANDLE, 0xFFFF_FFFF);
			PostMessageW(hwnd as HWND, WM_APP_PANE_EXIT, id as usize, 0);
		});
	}

	fn push_tab(
		&mut self,
		pane: Pane,
		command: String,
		envs: Vec<(String, String)>,
		label: String,
		ai: bool,
		kind: &'static str,
	) {
		let id = pane.id;
		let mut tab = Tab::new_with_env(pane, command.clone(), envs, label.clone());
		if ai {
			tab = tab.ai_opened();
		}
		self.tabs.push(tab);
		self.active_tab = self.tabs.len() - 1;
		self.record(Event::WindowOpened {
			window: id,
			kind,
			label,
		});
		if self.tabs[self.active_tab]
			.panes
			.first()
			.map(Pane::is_terminal)
			.unwrap_or(false)
		{
			self.record(Event::CommandStarted {
				window: id,
				command,
			});
		}
		self.start_pane_reader(id);
		self.resize_panes();
		self.invalidate();
	}

	fn push_tab_command(
		&mut self,
		command: String,
		envs: Vec<(String, String)>,
		label: String,
		ai: bool,
	) {
		let id = self.next_pane_id;
		self.next_pane_id += 1;
		let pane = match Pane::new_with_env(id, &command, 80, 24, &envs) {
			Ok(pane) => pane,
			Err(err) => {
				eprintln!("delta-term: cannot create tab: {err}");
				return;
			}
		};
		self.push_tab(pane, command, envs, label, ai, "terminal");
	}

	fn split(&mut self, dir: Dir) {
		if !matches!(self.active_kind(), Some(ActiveKind::Terminal)) {
			return;
		}
		let target = match self.active_tab().and_then(|tab| tab.active) {
			Some(id) => id,
			None => return,
		};
		let (command, envs) = match self.active_tab() {
			Some(tab) => (tab.command.clone(), tab.envs.clone()),
			None => return,
		};
		let id = self.next_pane_id;
		self.next_pane_id += 1;
		let pane = match Pane::new_with_env(id, &command, 80, 24, &envs) {
			Ok(pane) => pane,
			Err(err) => {
				eprintln!("delta-term: cannot split: {err}");
				return;
			}
		};
		if let Some(tab) = self.active_tab_mut() {
			tab.split(target, pane, dir);
		}
		self.record(Event::WindowOpened {
			window: id,
			kind: "terminal",
			label: self.label.clone(),
		});
		self.record(Event::CommandStarted {
			window: id,
			command,
		});
		self.start_pane_reader(id);
		self.resize_panes();
		self.invalidate();
	}

	fn new_tab(&mut self) {
		self.push_tab_command(
			self.command.clone(),
			Vec::new(),
			self.label.clone(),
			false,
		);
	}

	fn ensure_server(&mut self) -> bool {
		if self.server.is_none() {
			match Server::start() {
				Ok(server) => self.server = Some(server),
				Err(err) => {
					eprintln!("delta-term: shared opencode server unavailable: {err}");
					return false;
				}
			}
		}
		true
	}

	fn new_even_tab(&mut self) {
		let _ = self.ensure_server();
		let client = self.server.as_ref().map(|server| server.client.clone());
		let cwd = std::env::current_dir()
			.map(|path| path.to_string_lossy().into_owned())
			.unwrap_or_default();
		let id = self.next_pane_id;
		self.next_pane_id += 1;
		let pane = Pane::group(id, client, cwd);
		self.push_tab(
			pane,
			String::new(),
			Vec::new(),
			String::from("even"),
			false,
			"group",
		);
	}

	fn new_opencode_tab(&mut self) {
		let _ = self.ensure_server();
		let opencode = opencode_path();
		let (command, envs) = match self.server.as_ref() {
			Some(server) => (
				format!(
					"{} --server {}",
					quote_path(&opencode),
					server.client.url
				),
				server.client.env(),
			),
			None => (quote_path(&opencode), Vec::new()),
		};
		self.push_tab_command(command, envs, String::from("opencode"), false);
	}

	fn open_member_page(&mut self, name: &str, session: &str) {
		if let Some(index) = self
			.tabs
			.iter()
			.position(|tab| tab.command.contains(session))
		{
			self.active_tab = index;
			self.resize_panes();
			self.invalidate();
			return;
		}
		let _ = self.ensure_server();
		let opencode = opencode_path();
		let (command, envs) = match self.server.as_ref() {
			Some(server) => (
				format!(
					"{} --server {} --session {}",
					quote_path(&opencode),
					server.client.url,
					session
				),
				server.client.env(),
			),
			None => return,
		};
		let label = if name.is_empty() {
			String::from("AI")
		} else {
			String::from(name)
		};
		self.push_tab_command(command, envs, label, true);
	}

	fn open_editor_path(&mut self, path: String) {
		let id = self.next_pane_id;
		self.next_pane_id += 1;
		let path_buf = PathBuf::from(&path);
		let pane = match Pane::editor(id, path_buf, self.hwnd as usize) {
			Ok(pane) => pane,
			Err(err) => {
				eprintln!("delta-term: cannot open {path}: {err}");
				return;
			}
		};
		let label = pane
			.editor_ref()
			.map(|editor| editor.title())
			.unwrap_or_else(|| path.clone());
		self.push_tab(pane, String::new(), Vec::new(), label, false, "editor");
	}

	fn open_file_dialog(&mut self) {
		if let Some(path) = pick_file(self.hwnd) {
			self.open_editor_path(path);
		}
	}

	fn close_session_tabs(&mut self, sessions: &[String]) {
		let mut indexes: Vec<usize> = self
			.tabs
			.iter()
			.enumerate()
			.filter(|(_, tab)| {
				sessions
					.iter()
					.any(|session| tab.command.contains(session))
			})
			.map(|(index, _)| index)
			.collect();
		indexes.sort_unstable_by(|a, b| b.cmp(a));
		for index in indexes {
			self.close_tab_index(index);
		}
	}

	fn close_pane(&mut self, id: u64) {
		let tab_index = match self
			.tabs
			.iter()
			.position(|tab| tab.contains_pane(id))
		{
			Some(index) => index,
			None => return,
		};
		let mut sessions = Vec::new();
		if let Some(pane) = self.tabs[tab_index]
			.panes
			.iter()
			.find(|pane| pane.id == id)
		{
			if let Some(group) = pane.group_ref() {
				for (member, session) in group.sessions() {
					self.record(Event::AiStopped {
						member,
						session: session.clone(),
					});
					sessions.push(session);
				}
				group.stop_all();
			}
		}
		let pane = self.tabs[tab_index].remove_pane(id);
		self.record(Event::WindowClosed { window: id });
		if pane.is_none() {
			return;
		}
		drop(pane);
		if self.tabs[tab_index].is_empty() {
			self.close_tab_index(tab_index);
		} else if tab_index == self.active_tab {
			self.resize_panes();
			self.invalidate();
		}
		if !sessions.is_empty() {
			self.close_session_tabs(&sessions);
		}
	}

	fn close_tab_index(&mut self, index: usize) {
		if index >= self.tabs.len() {
			return;
		}
		let tab = self.tabs.remove(index);
		let mut sessions = Vec::new();
		for pane in &tab.panes {
			self.record(Event::WindowClosed { window: pane.id });
			if let Some(group) = pane.group_ref() {
				for (member, session) in group.sessions() {
					self.record(Event::AiStopped {
						member,
						session: session.clone(),
					});
					sessions.push(session);
				}
				group.stop_all();
			}
		}
		drop(tab);
		if self.tabs.is_empty() {
			unsafe {
				DestroyWindow(self.hwnd);
			}
			return;
		}
		if index < self.active_tab {
			self.active_tab -= 1;
		} else if self.active_tab >= self.tabs.len() {
			self.active_tab = self.tabs.len() - 1;
		}
		self.resize_panes();
		if !sessions.is_empty() {
			self.close_session_tabs(&sessions);
		}
		self.invalidate();
	}

	fn close_active(&mut self) {
		if let Some(id) = self.active_pane().map(|pane| pane.id) {
			self.close_pane(id);
		}
	}

	fn switch_tab(&mut self, delta: isize) {
		if self.tabs.is_empty() {
			return;
		}
		let len = self.tabs.len() as isize;
		self.active_tab = ((self.active_tab as isize + delta).rem_euclid(len)) as usize;
		self.resize_panes();
		self.invalidate();
	}

	fn switch_tab_at(&mut self, index: usize) {
		if index < self.tabs.len() {
			self.active_tab = index;
			self.resize_panes();
			self.invalidate();
		}
	}

	fn rects(&self) -> Vec<(u64, RECT)> {
		if self.hwnd.is_null() {
			return Vec::new();
		}
		let mut client: RECT = unsafe { zeroed() };
		if unsafe { GetClientRect(self.hwnd, &mut client) } == 0 {
			return Vec::new();
		}
		let mut out = Vec::new();
		if let Some(tab) = self.active_tab() {
			if let Some(root) = tab.layout.as_ref() {
				root.rects(self.content_rect(client), &mut out);
			}
		}
		out
	}

	fn resize_panes(&mut self) {
		if self.cell_w <= 0 || self.cell_h <= 0 {
			return;
		}
		let sizes: Vec<(u64, usize, usize)> = self
			.rects()
			.into_iter()
			.map(|(id, rect)| (id, self.cols_for(rect), self.rows_for(rect)))
			.collect();
		let Some(tab) = self.tabs.get_mut(self.active_tab) else {
			return;
		};
		for (id, cols, rows) in sizes {
			let Some(pane) = tab.panes.iter_mut().find(|pane| pane.id == id) else {
				continue;
			};
			match &mut pane.kind {
				PaneKind::Terminal { pty, term, .. } => {
					let current = match term.lock() {
						Ok(term) => (term.active().cols, term.active().rows),
						Err(_) => continue,
					};
					if current != (cols, rows) {
						if let Ok(mut term) = term.lock() {
							term.resize(cols, rows);
						}
						pty.resize(cols, rows);
					}
				}
				PaneKind::Editor(editor) => {
					editor.ensure_visible(rows.saturating_sub(1).max(1));
				}
				PaneKind::Group(_) => {}
			}
		}
	}

	fn focus_at(&mut self, x: i32, y: i32) {
		if y < self.tab_bar_height() {
			let index = (x.max(0) / 180) as usize;
			self.switch_tab_at(index);
			return;
		}
		for (id, rect) in self.rects() {
			if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
				if let Some(tab) = self.active_tab_mut() {
					tab.active = Some(id);
				}
				self.invalidate();
				return;
			}
		}
	}

	fn focus_dir(&mut self, dx: i32, dy: i32) {
		let active = match self.active_tab().and_then(|tab| tab.active) {
			Some(id) => id,
			None => return,
		};
		let rects = self.rects();
		let current = match rects.iter().find(|(id, _)| *id == active) {
			Some((_, rect)) => *rect,
			None => return,
		};
		let cx = (current.left + current.right) / 2;
		let cy = (current.top + current.bottom) / 2;
		let mut best: Option<(u64, i32)> = None;
		for (id, rect) in rects {
			if id == active {
				continue;
			}
			let px = (rect.left + rect.right) / 2;
			let py = (rect.top + rect.bottom) / 2;
			let ok = match (dx, dy) {
				(-1, 0) => px < cx,
				(1, 0) => px > cx,
				(0, -1) => py < cy,
				(0, 1) => py > cy,
				_ => false,
			};
			if !ok {
				continue;
			}
			let distance = (px - cx).abs() + (py - cy).abs();
			if best.map(|(_, d)| distance < d).unwrap_or(true) {
				best = Some((id, distance));
			}
		}
		if let Some((id, _)) = best {
			if let Some(tab) = self.active_tab_mut() {
				tab.active = Some(id);
			}
			self.invalidate();
		}
	}

	fn click_at(&mut self, x: i32, y: i32) {
		if y < self.tab_bar_height() {
			let index = (x.max(0) / 180) as usize;
			self.switch_tab_at(index);
			return;
		}
		let Some((id, rect)) = self
			.rects()
			.into_iter()
			.find(|(_, rect)| x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom)
		else {
			return;
		};
		if let Some(tab) = self.tabs.get_mut(self.active_tab) {
			tab.active = Some(id);
		}
		let kind = match self
			.active_tab()
			.and_then(|tab| tab.panes.iter().find(|pane| pane.id == id))
		{
			Some(pane) if pane.group_ref().is_some() => Some(ActiveKind::Group),
			Some(pane) if pane.editor_ref().is_some() => Some(ActiveKind::Editor),
			Some(_) => Some(ActiveKind::Terminal),
			None => None,
		};
		let rows = self.rows_for(rect);
		let cols = self.cols_for(rect);
		let row = ((y - rect.top - 1).max(0) / self.cell_h.max(1)) as usize;
		match kind {
			Some(ActiveKind::Editor) => {
				let text_rows = rows.saturating_sub(1).max(1);
				let gutter = self
					.active_tab()
					.and_then(|tab| tab.panes.iter().find(|pane| pane.id == id))
					.and_then(|pane| pane.editor_ref())
					.map(gutter_cols)
					.unwrap_or(0);
				let cell = ((x - rect.left - 1).max(0) / self.cell_w.max(1)) as usize;
				if row < text_rows && cell >= gutter {
					let mut line = Vec::new();
					if let Some(editor) = self.active_editor_mut() {
						if let Some(raw) = editor.buffer.lines.get(editor.scroll + row) {
							line = raw.clone();
						}
					}
					let col = char_at_cells(&line, cell - gutter);
					if let Some(editor) = self.active_editor_mut() {
						editor.click(col, row, text_rows);
					}
				}
			}
			Some(ActiveKind::Group) => {
				let left = group_left(cols);
				if x < rect.left + 1 + left as i32 * self.cell_w && row < rows {
					let action = self.active_group_mut().and_then(|group| group.click(row));
					if let Some(action) = action {
						self.handle_group_actions(vec![action]);
					}
				}
			}
			_ => {}
		}
		self.invalidate();
	}

	fn handle_group_actions(&mut self, actions: Vec<GroupAction>) {
		for action in actions {
			match action {
				GroupAction::Open { name, session } => self.open_member_page(&name, &session),
				GroupAction::Close { session } => self.close_session_tabs(&[session]),
			}
		}
	}

	fn drain_group_events(&self) {
		let mut events = Vec::new();
		for tab in &self.tabs {
			for pane in &tab.panes {
				if let Some(group) = pane.group_ref() {
					events.extend(group.take_events());
				}
			}
		}
		for event in events {
			self.record(event);
		}
	}

	fn poll_editors(&mut self) -> bool {
		let mut changed = false;
		for tab in &mut self.tabs {
			for pane in &mut tab.panes {
				if let Some(editor) = pane.editor_mut() {
					if editor.poll() {
						changed = true;
					}
				}
			}
		}
		changed
	}

	fn has_group(&self) -> bool {
		self.tabs
			.iter()
			.any(|tab| tab.panes.iter().any(|pane| pane.group_ref().is_some()))
	}

	fn tick(&mut self) {
		self.drain_group_events();
		let changed = self.poll_editors();
		if changed || self.has_group() {
			self.invalidate();
		}
	}

	fn init_gdi(&mut self, hwnd: HWND) {
		unsafe {
			let screen = GetDC(hwnd);
			if screen.is_null() {
				return;
			}
			let mem = CreateCompatibleDC(screen);
			let bmp = CreateCompatibleBitmap(screen, 1000, 640);
			let old = SelectObject(mem, bmp);
			ReleaseDC(hwnd, screen);

			let face = wide("Consolas");
			let font = CreateFontW(
				-16,
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
				face.as_ptr(),
			);
			SelectObject(mem, font as HGDIOBJ);
			SetBkMode(mem, TRANSPARENT as i32);

			self.mem_dc = mem;
			self.mem_bmp = bmp;
			self.old_bmp = old;
			self.font = font;
			self.default_brush = CreateSolidBrush(rgb(14, 14, 18));
			self.resize_backbuffer(hwnd);
		}
	}

	fn resize_backbuffer(&mut self, hwnd: HWND) {
		unsafe {
			let mut rect: RECT = zeroed();
			if GetClientRect(hwnd, &mut rect) == 0 {
				return;
			}
			let width = rect.right - rect.left;
			let height = rect.bottom - rect.top;
			if width <= 0 || height <= 0 {
				return;
			}

			let screen = GetDC(hwnd);
			if screen.is_null() {
				return;
			}
			let mem = CreateCompatibleDC(screen);
			let bmp = CreateCompatibleBitmap(screen, width, height);
			let old = SelectObject(mem, bmp);
			ReleaseDC(hwnd, screen);
			SelectObject(mem, self.font as HGDIOBJ);
			SetBkMode(mem, TRANSPARENT as i32);

			if !self.mem_dc.is_null() {
				SelectObject(self.mem_dc, self.old_bmp);
				DeleteObject(self.mem_bmp);
				DeleteDC(self.mem_dc);
			}
			self.mem_dc = mem;
			self.mem_bmp = bmp;
			self.old_bmp = old;

			let mut size = SIZE { cx: 0, cy: 0 };
			let m = wide("M");
			let ok = GetTextExtentPoint32W(mem, m.as_ptr(), 1, &mut size);
			self.cell_w = if ok != 0 && size.cx > 0 { size.cx } else { 8 };
			self.cell_h = if ok != 0 && size.cy > 0 { size.cy } else { 16 };
		}
	}

	fn render_tab_bar(&self, client: RECT) {
		if self.tabs.is_empty() {
			return;
		}
		for (index, rect) in self.tab_rects(client) {
			let active = index == self.active_tab;
			self.fill(
				rect,
				if active { (40, 40, 46) } else { (22, 22, 26) },
			);
			let label = match self.tabs.get(index) {
				Some(tab) => format!("{}  {}", index + 1, tab.label),
				None => String::new(),
			};
			let width = ((rect.right - rect.left - 12).max(1) / self.cell_w.max(1)) as usize;
			self.draw_text(
				rect.left + 6,
				rect.top + 4,
				&truncate_text(&label, width),
				if active { theme::EDITOR_FG } else { theme::DIM },
			);
			if active {
				self.fill(
					RECT {
						left: rect.left,
						top: rect.bottom - 2,
						right: rect.right,
						bottom: rect.bottom,
					},
					theme::ACCENT,
				);
			}
		}
	}

	fn load_initial_image(&mut self) {
		let path = match self.image_path.as_ref() {
			Some(path) => path.clone(),
			None => return,
		};
		let (width, height, rgba) = match crate::vt::load_png(&path) {
			Ok(image) => image,
			Err(err) => {
				eprintln!("delta-term: cannot load image: {err}");
				return;
			}
		};
		if let Some(pane) = self.active_pane() {
			if let Some(term) = pane.term() {
				if let Ok(mut term) = term.lock() {
					term.set_image(
						Some(1),
						width,
						height,
						rgba,
						Some(crate::term::ImagePlacement {
							x: 0,
							y: 1,
							cols: Some(30),
							rows: Some(15),
						}),
					);
				}
			}
			self.invalidate();
		}
	}

	fn paint(&mut self, hwnd: HWND) {
		unsafe {
			let mut ps: PAINTSTRUCT = zeroed();
			let screen_dc = BeginPaint(hwnd, &mut ps);
			if self.mem_dc.is_null() {
				self.init_gdi(hwnd);
			}
			if self.mem_dc.is_null() {
				EndPaint(hwnd, &ps);
				return;
			}

			let mut rect: RECT = zeroed();
			GetClientRect(hwnd, &mut rect);
			FillRect(self.mem_dc, &rect, self.default_brush);
			self.render_tab_bar(rect);

			let active = self.active_tab().and_then(|tab| tab.active);
			if let Some(tab) = self.tabs.get(self.active_tab) {
				for (id, pane_rect) in self.rects() {
					if let Some(pane) = tab.panes.iter().find(|pane| pane.id == id) {
						self.render_pane(pane, pane_rect, active == Some(id));
					}
				}
			}

			BitBlt(
				screen_dc,
				0,
				0,
				rect.right - rect.left,
				rect.bottom - rect.top,
				self.mem_dc,
				0,
				0,
				SRCCOPY,
			);
			EndPaint(hwnd, &ps);
		}
	}

	fn render_pane(&self, pane: &Pane, rect: RECT, active: bool) {
		match &pane.kind {
			PaneKind::Terminal { .. } => self.render_terminal(pane, rect, active),
			PaneKind::Editor(editor) => self.render_editor(editor, rect, active),
			PaneKind::Group(group) => self.render_group(group, rect, active),
		}
	}

	fn render_terminal(&self, pane: &Pane, rect: RECT, active: bool) {
		self.draw_border(
			rect,
			if active { (110, 110, 110) } else { (55, 55, 55) },
		);
		let term_arc = match pane.term() {
			Some(term) => term,
			None => return,
		};
		let term = match term_arc.lock() {
			Ok(term) => term,
			Err(_) => return,
		};
		let screen = term.active();
		let cols = self.cols_for(rect);
		let rows = self.rows_for(rect);

		for y in 0..rows.min(screen.rows) {
			for x in 0..cols.min(screen.cols) {
				let cell = screen.view_cell(x, y, term.scroll_offset);
				if cell.width == 0 {
					continue;
				}
				let attrs = cell.attrs;
				let mut fg = colorref(attrs.fg, false, attrs.bold);
				let mut bg = colorref(attrs.bg, true, attrs.bold);
				if attrs.reverse {
					std::mem::swap(&mut fg, &mut bg);
				}
				let fg_tuple = (fg as u8, (fg >> 8) as u8, (fg >> 16) as u8);
				let px = rect.left + x as i32 * self.cell_w + 1;
				let py = rect.top + y as i32 * self.cell_h + 1;
				if bg != rgb(0, 0, 0) {
					self.fill(
						RECT {
							left: px,
							top: py,
							right: px + self.cell_w * cell.width as i32,
							bottom: py + self.cell_h,
						},
						(bg as u8, (bg >> 8) as u8, (bg >> 16) as u8),
					);
				}
				if cell.ch != ' ' && cell.ch != '\0' {
					self.draw_ch(px, py, cell.ch, fg_tuple);
				}
				if attrs.underline && cell.ch != ' ' {
					self.fill(
						RECT {
							left: px,
							top: py + self.cell_h - 2,
							right: px + self.cell_w * cell.width as i32,
							bottom: py + self.cell_h - 1,
						},
						fg_tuple,
					);
				}
			}
		}

		for image in &term.images {
			if let Some(place) = image.placement {
				let cols = place.cols.unwrap_or_else(|| {
					((image.width + self.cell_w as usize - 1) / self.cell_w as usize).max(1)
				});
				let rows = place.rows.unwrap_or_else(|| {
					((image.height + self.cell_h as usize - 1) / self.cell_h as usize).max(1)
				});
				let x = rect.left + 1 + place.x as i32 * self.cell_w;
				let y = rect.top + 1 + place.y as i32 * self.cell_h;
				self.draw_image(
					image,
					x,
					y,
					cols as i32 * self.cell_w,
					rows as i32 * self.cell_h,
				);
			}
		}

		if active && term.cursor_visible && term.scroll_offset == 0 {
			let x = screen.cursor_x;
			let y = screen.cursor_y;
			if x < cols && y < rows {
				let px = rect.left + x as i32 * self.cell_w + 1;
				let py = rect.top + y as i32 * self.cell_h + 1;
				self.fill(
					RECT {
						left: px,
						top: py,
						right: px + self.cell_w,
						bottom: py + self.cell_h,
					},
					theme::CURSOR_BG,
				);
				let cell = screen.cell(x, y);
				if cell.ch != ' ' && cell.ch != '\0' {
					self.draw_ch(px, py, cell.ch, theme::EDITOR_BG);
				}
			}
		}
	}

	fn render_editor(&self, editor: &Editor, rect: RECT, active: bool) {
		self.draw_border(
			rect,
			if active { theme::ACCENT } else { (55, 55, 55) },
		);
		let cols = self.cols_for(rect);
		let pane_rows = self.rows_for(rect);
		let text_rows = pane_rows.saturating_sub(1).max(1);
		let gutter = gutter_cols(editor);
		let status_top = rect.top + 1 + text_rows as i32 * self.cell_h;
		let inner = RECT {
			left: rect.left + 1,
			top: rect.top + 1,
			right: rect.right - 1,
			bottom: status_top,
		};
		self.fill(inner, theme::EDITOR_BG);
		self.fill(
			RECT {
				left: rect.left + 1,
				top: rect.top + 1,
				right: rect.left + 1 + gutter as i32 * self.cell_w,
				bottom: status_top,
			},
			theme::GUTTER_BG,
		);

		for row in 0..text_rows {
			let line_index = editor.scroll + row;
			if line_index >= editor.buffer.lines.len() {
				break;
			}
			let y = rect.top + 1 + row as i32 * self.cell_h;
			let number = format!(
				"{:>width$}",
				line_index + 1,
				width = gutter.saturating_sub(2)
			);
			self.draw_text(rect.left + 1 + self.cell_w, y, &number, theme::GUTTER_FG);
			let line = &editor.buffer.lines[line_index];
			let mut cell = 0usize;
			for (col, &ch) in line.iter().enumerate() {
				let step = if ch == '\t' {
					4 - (cell % 4)
				} else {
					display_width(ch)
				};
				if gutter + cell >= cols {
					break;
				}
				if ch != '\t' {
					let x = rect.left + 1 + (gutter + cell) as i32 * self.cell_w;
					self.draw_ch(
						x,
						y,
						ch,
						theme::fg(editor.marks.category(line_index, col)),
					);
					if let Some(severity) = editor.diagnostic_at(line_index, col) {
						let color = if severity <= 1 {
							theme::ERROR
						} else {
							theme::WARN
						};
						self.fill(
							RECT {
								left: x,
								top: y + self.cell_h - 2,
								right: x + step as i32 * self.cell_w,
								bottom: y + self.cell_h - 1,
							},
							color,
						);
					}
				}
				cell += step;
			}
		}

		if active && editor.buffer.line >= editor.scroll {
			let row = editor.buffer.line - editor.scroll;
			if row < text_rows {
				let line = &editor.buffer.lines[editor.buffer.line];
				let cell = cells_width(line, editor.buffer.col);
				let x = rect.left + 1 + (gutter + cell) as i32 * self.cell_w;
				let y = rect.top + 1 + row as i32 * self.cell_h;
				self.fill(
					RECT {
						left: x,
						top: y,
						right: x + self.cell_w,
						bottom: y + self.cell_h,
					},
					theme::CURSOR_BG,
				);
				if let Some(&ch) = line.get(editor.buffer.col) {
					if ch != '\t' {
						self.draw_ch(x, y, ch, theme::EDITOR_BG);
					}
				}
			}
		}

		let status = format!(
			"{}  {}  {}:{}  {}  {}",
			editor.title(),
			editor.language.name(),
			editor.buffer.line + 1,
			editor.buffer.col + 1,
			editor.buffer.lines.len(),
			editor.status
		);
		self.fill(
			RECT {
				left: rect.left + 1,
				top: status_top,
				right: rect.right - 1,
				bottom: rect.bottom - 1,
			},
			theme::STATUS_BG,
		);
		self.draw_text(
			rect.left + 1 + self.cell_w / 2,
			status_top,
			&truncate_text(&status, cols.saturating_sub(1)),
			theme::STATUS_FG,
		);

		if active && editor.completion_open && !editor.completions.is_empty() {
			let count = editor.completions.len().min(8);
			let width_chars = editor
				.completions
				.iter()
				.take(count)
				.map(|item| display_width_str(&item.label) + 2 + display_width_str(&item.detail))
				.max()
				.unwrap_or(20)
				.clamp(20, 48);
			let width = ((width_chars as i32 + 1) * self.cell_w)
				.min(rect.right - rect.left - 2)
				.max(self.cell_w);
			let height = count as i32 * self.cell_h + 2;
			let caret_row = editor.buffer.line.saturating_sub(editor.scroll);
			let line = &editor.buffer.lines[editor.buffer.line];
			let caret_cells = cells_width(line, editor.buffer.col);
			let cx = rect.left + 1 + (gutter + caret_cells) as i32 * self.cell_w;
			let mut cy = rect.top + 1 + (caret_row as i32 + 1) * self.cell_h;
			if cy + height > status_top {
				cy = (status_top - height).max(rect.top + 1);
			}
			let max_left = (rect.right - 2 - width).max(rect.left + 1);
			let left = cx.min(max_left);
			let panel = RECT {
				left,
				top: cy,
				right: left + width,
				bottom: cy + height,
			};
			self.fill(panel, theme::PANEL_BG);
			self.draw_border(panel, theme::PANEL_BORDER);
			for (index, item) in editor.completions.iter().take(count).enumerate() {
				let row_rect = RECT {
					left: panel.left + 1,
					top: panel.top + 1 + index as i32 * self.cell_h,
					right: panel.right - 1,
					bottom: panel.top + 1 + (index as i32 + 1) * self.cell_h,
				};
				if index == editor.completion_index {
					self.fill(row_rect, theme::SELECT_BG);
				}
				let text = truncate_text(&format!("{}  {}", item.label, item.detail), width_chars);
				self.draw_text(row_rect.left, row_rect.top, &text, theme::fg(theme::Category::Text));
			}
		}
	}

	fn render_group(&self, group: &GroupPane, rect: RECT, active: bool) {
		self.draw_border(
			rect,
			if active { theme::ACCENT } else { (55, 55, 55) },
		);
		let cols = self.cols_for(rect);
		let rows = self.rows_for(rect);
		let view = group.view(cols, rows);
		let left = view.left;
		let stream_x = rect.left + 1 + (left as i32 + 2) * self.cell_w;
		let stream_width = cols.saturating_sub(left + 3).max(8);

		self.fill(
			RECT {
				left: rect.left + 1,
				top: rect.top + 1,
				right: rect.right - 1,
				bottom: rect.top + 1 + self.cell_h,
			},
			theme::PANEL_BG,
		);
		self.draw_text(
			rect.left + 1 + self.cell_w / 2,
			rect.top + 1,
			&truncate_text(&view.title, cols.saturating_sub(1)),
			theme::ACCENT,
		);
		self.fill(
			RECT {
				left: rect.left + 1,
				top: rect.top + 1 + self.cell_h,
				right: rect.right - 1,
				bottom: rect.top + 2 + self.cell_h,
			},
			theme::PANEL_BORDER,
		);

		let side_top = rect.top + 1 + 2 * self.cell_h;
		let side_right = rect.left + 1 + left as i32 * self.cell_w;
		self.fill(
			RECT {
				left: rect.left + 1,
				top: side_top,
				right: side_right,
				bottom: rect.bottom - 1,
			},
			theme::PANEL_BG,
		);
		self.fill(
			RECT {
				left: side_right,
				top: side_top,
				right: side_right + 1,
				bottom: rect.bottom - 1,
			},
			theme::PANEL_BORDER,
		);
		for (index, member) in view.members.iter().enumerate() {
			let y = side_top + index as i32 * self.cell_h;
			if y + self.cell_h > rect.bottom - 1 {
				break;
			}
			if member.selected {
				self.fill(
					RECT {
						left: rect.left + 1,
						top: y,
						right: side_right,
						bottom: y + self.cell_h,
					},
					theme::SELECT_BG,
				);
			}
			let color = if member.busy {
				theme::ACCENT
			} else if member.text.starts_with('!') || member.text.starts_with('*') {
				theme::WARN
			} else {
				theme::STATUS_FG
			};
			self.draw_text(
				rect.left + 1 + self.cell_w / 2,
				y,
				&truncate_text(&member.text, left.saturating_sub(1)),
				color,
			);
		}

		for (index, line) in view.lines.iter().enumerate() {
			let y = side_top + index as i32 * self.cell_h;
			if y + self.cell_h > rect.bottom - 1 {
				break;
			}
			let color = match line.kind {
				RecordKind::Human => theme::ACCENT,
				RecordKind::Reply => theme::EDITOR_FG,
				RecordKind::Suggestion => theme::DIM,
				RecordKind::System => theme::DIM,
				RecordKind::Error => theme::ERROR,
				RecordKind::Stopped => theme::WARN,
			};
			self.draw_text(
				stream_x,
				y,
				&truncate_text(&line.text, stream_width),
				color,
			);
		}

		let hint_y = rect.top + 1 + rows.saturating_sub(3) as i32 * self.cell_h;
		self.draw_text(
			stream_x,
			hint_y,
			&truncate_text(&view.hint, stream_width),
			theme::DIM,
		);

		let input_y = rect.top + 1 + rows.saturating_sub(2) as i32 * self.cell_h;
		self.fill(
			RECT {
				left: stream_x - self.cell_w / 2,
				top: input_y,
				right: rect.right - 1,
				bottom: input_y + self.cell_h,
			},
			theme::PANEL_BG,
		);
		self.draw_text(stream_x, input_y, ">", theme::ACCENT);
		let input_x = stream_x + 2 * self.cell_w;
		self.draw_text(
			input_x,
			input_y,
			&truncate_text(&view.input, cols.saturating_sub(left + 4)),
			theme::EDITOR_FG,
		);
		if active {
			let cursor_cells: usize = view
				.input
				.chars()
				.take(view.cursor)
				.map(display_width)
				.sum();
			let cx = input_x + cursor_cells as i32 * self.cell_w;
			self.fill(
				RECT {
					left: cx,
					top: input_y,
					right: cx + self.cell_w,
					bottom: input_y + self.cell_h,
				},
				theme::CURSOR_BG,
			);
		}

		if let Some(picker) = view.picker.as_ref() {
			let count = picker.items.len().min(8);
			let width_chars = picker
				.items
				.iter()
				.take(count)
				.map(|item| display_width_str(&item.text))
				.max()
				.unwrap_or(16)
				.clamp(16, 48);
			let width = ((width_chars as i32 + 1) * self.cell_w)
				.min(rect.right - stream_x - 2)
				.max(self.cell_w);
			let height = count as i32 * self.cell_h + 2;
			let bottom = input_y - 2;
			let top = (bottom - height).max(rect.top + 2 + self.cell_h);
			let panel = RECT {
				left: stream_x,
				top,
				right: stream_x + width,
				bottom,
			};
			self.fill(panel, theme::PANEL_BG);
			self.draw_border(panel, theme::PANEL_BORDER);
			for (index, item) in picker.items.iter().take(count).enumerate() {
				let y = panel.top + 1 + index as i32 * self.cell_h;
				if item.selected {
					self.fill(
						RECT {
							left: panel.left + 1,
							top: y,
							right: panel.right - 1,
							bottom: y + self.cell_h,
						},
						theme::SELECT_BG,
					);
				}
				self.draw_text(
					panel.left + self.cell_w / 2,
					y,
					&truncate_text(&item.text, width_chars),
					theme::EDITOR_FG,
				);
			}
		}
	}

	fn draw_image(&self, image: &Image, x: i32, y: i32, width: i32, height: i32) {
		if image.width == 0
			|| image.height == 0
			|| image.rgba.len() < image.width * image.height * 4
		{
			return;
		}
		let mut bgra = Vec::with_capacity(image.rgba.len());
		for chunk in image.rgba.chunks_exact(4) {
			bgra.push(chunk[2]);
			bgra.push(chunk[1]);
			bgra.push(chunk[0]);
			bgra.push(chunk[3]);
		}
		let mut info: BITMAPINFO = unsafe { zeroed() };
		info.bmiHeader = BITMAPINFOHEADER {
			biSize: size_of::<BITMAPINFOHEADER>() as u32,
			biWidth: image.width as i32,
			biHeight: -(image.height as i32),
			biPlanes: 1,
			biBitCount: 32,
			biCompression: BI_RGB,
			biSizeImage: 0,
			biXPelsPerMeter: 0,
			biYPelsPerMeter: 0,
			biClrUsed: 0,
			biClrImportant: 0,
		};
		unsafe {
			StretchDIBits(
				self.mem_dc,
				x,
				y,
				width,
				height,
				0,
				0,
				image.width as i32,
				image.height as i32,
				bgra.as_ptr() as *const c_void,
				&info,
				DIB_RGB_COLORS,
				SRCCOPY,
			);
		}
	}

	fn fill(&self, rect: RECT, color: (u8, u8, u8)) {
		unsafe {
			let brush = CreateSolidBrush(rgb(color.0, color.1, color.2));
			FillRect(self.mem_dc, &rect, brush);
			DeleteObject(brush);
		}
	}

	fn draw_text(&self, x: i32, y: i32, text: &str, color: (u8, u8, u8)) {
		if text.is_empty() {
			return;
		}
		unsafe {
			SetTextColor(self.mem_dc, rgb(color.0, color.1, color.2));
			let text = wide(text);
			TextOutW(self.mem_dc, x, y, text.as_ptr(), (text.len() - 1) as i32);
		}
	}

	fn draw_ch(&self, x: i32, y: i32, ch: char, color: (u8, u8, u8)) {
		unsafe {
			SetTextColor(self.mem_dc, rgb(color.0, color.1, color.2));
			let mut buf = [0u16; 2];
			let n = ch.encode_utf16(&mut buf).len() as i32;
			TextOutW(self.mem_dc, x, y, buf.as_ptr(), n);
		}
	}

	fn draw_border(&self, rect: RECT, color: (u8, u8, u8)) {
		let edges = [
			RECT {
				left: rect.left,
				top: rect.top,
				right: rect.right,
				bottom: rect.top + 1,
			},
			RECT {
				left: rect.left,
				top: rect.bottom - 1,
				right: rect.right,
				bottom: rect.bottom,
			},
			RECT {
				left: rect.left,
				top: rect.top,
				right: rect.left + 1,
				bottom: rect.bottom,
			},
			RECT {
				left: rect.right - 1,
				top: rect.top,
				right: rect.right,
				bottom: rect.bottom,
			},
		];
		for edge in edges {
			self.fill(edge, color);
		}
	}

	fn caret_position(&self) -> Option<(i32, i32)> {
		let id = self.active_tab().and_then(|tab| tab.active)?;
		let rect = self
			.rects()
			.into_iter()
			.find(|(pane, _)| *pane == id)?
			.1;
		let pane = self
			.active_tab()?
			.panes
			.iter()
			.find(|pane| pane.id == id)?;
		match &pane.kind {
			PaneKind::Terminal { term, .. } => {
				let term = term.lock().ok()?;
				let screen = term.active();
				Some((
					rect.left + 1 + screen.cursor_x as i32 * self.cell_w,
					rect.top + 1 + screen.cursor_y as i32 * self.cell_h,
				))
			}
			PaneKind::Editor(editor) => {
				let row = editor.buffer.line.saturating_sub(editor.scroll);
				let gutter = gutter_cols(editor);
				let line = editor.buffer.lines.get(editor.buffer.line)?;
				let cells = cells_width(line, editor.buffer.col);
				Some((
					rect.left + 1 + (gutter + cells) as i32 * self.cell_w,
					rect.top + 1 + row as i32 * self.cell_h,
				))
			}
			PaneKind::Group(group) => {
				let cols = self.cols_for(rect);
				let rows = self.rows_for(rect);
				let left = group_left(cols);
				let view = group.view(cols, rows);
				let cells: usize = view
					.input
					.chars()
					.take(view.cursor)
					.map(display_width)
					.sum();
				Some((
					rect.left + 1 + (left as i32 + 4) * self.cell_w + cells as i32 * self.cell_w,
					rect.top + 1 + rows.saturating_sub(2) as i32 * self.cell_h,
				))
			}
		}
	}

	fn ime_position(&mut self, hwnd: HWND) {
		let (x, y) = match self.caret_position() {
			Some(position) => position,
			None => return,
		};
		unsafe {
			let himc = ImmGetContext(hwnd);
			if himc.is_null() {
				return;
			}
			let form = COMPOSITIONFORM {
				dwStyle: CFS_POINT,
				ptCurrentPos: POINT { x, y },
				rcArea: zeroed(),
			};
			let _ = ImmSetCompositionWindow(himc, &form);
			let candidate = CANDIDATEFORM {
				dwIndex: 0,
				dwStyle: CFS_EXCLUDE,
				ptCurrentPos: POINT { x, y },
				rcArea: RECT {
					left: x,
					top: y,
					right: x + self.cell_w,
					bottom: y + self.cell_h,
				},
			};
			let _ = ImmSetCandidateWindow(himc, &candidate);
			ImmReleaseContext(hwnd, himc);
		}
	}

	fn on_ime_composition(&mut self, hwnd: HWND, lparam: LPARAM) {
		unsafe {
			let himc = ImmGetContext(hwnd);
			if himc.is_null() {
				return;
			}
			if lparam & (GCS_RESULTSTR as isize) != 0 {
				let text = ime_string(himc, GCS_RESULTSTR);
				if !text.is_empty() {
					self.insert_text(&text);
				}
			}
			if lparam & (GCS_COMPSTR as isize) != 0 {
				self.ime_position(hwnd);
			}
			ImmReleaseContext(hwnd, himc);
		}
	}

	fn write(&self, bytes: &[u8]) {
		if let Some(pane) = self.active_pane() {
			if let Some(pty) = pane.pty() {
				pty.write(bytes);
			}
		}
	}

	fn insert_text(&mut self, text: &str) {
		if text.is_empty() {
			return;
		}
		match self.active_kind() {
			Some(ActiveKind::Terminal) => self.write(text.as_bytes()),
			Some(ActiveKind::Editor) => {
				if let Some(editor) = self.active_editor_mut() {
					editor.insert_str(text);
				}
			}
			Some(ActiveKind::Group) => {
				if let Some(group) = self.active_group_mut() {
					group.insert_str(text);
				}
			}
			None => return,
		}
		self.invalidate();
	}

	fn paste(&mut self) {
		let text = match clipboard_text() {
			Some(text) => text,
			None => match clipboard_image_path() {
				Some(path) => format!("{path} "),
				None => return,
			},
		};
		match self.active_kind() {
			Some(ActiveKind::Terminal) => {
				let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
				self.write(normalized.as_bytes());
			}
			Some(ActiveKind::Editor) => {
				if let Some(editor) = self.active_editor_mut() {
					editor.insert_str(&text);
				}
			}
			Some(ActiveKind::Group) => {
				if let Some(group) = self.active_group_mut() {
					group.insert_str(&text);
				}
			}
			None => return,
		}
		self.invalidate();
	}

	fn scroll_active(&mut self, delta: isize) {
		if let Some(pane) = self.active_pane() {
			if let Some(term) = pane.term() {
				if let Ok(mut term) = term.lock() {
					term.scroll_view(delta);
				}
			}
		}
		self.invalidate();
	}

	fn wheel_at(&mut self, x: i32, y: i32, delta: i32) {
		if y < self.tab_bar_height() || delta == 0 {
			return;
		}
		self.focus_at(x, y);
		match self.active_kind() {
			Some(ActiveKind::Terminal) => self.scroll_active(delta as isize * 3),
			Some(ActiveKind::Editor) => {
				if let Some(editor) = self.active_editor_mut() {
					let max = editor.buffer.line_count().saturating_sub(1) as isize;
					let next = editor.scroll as isize - delta as isize * 3;
					editor.scroll = next.clamp(0, max) as usize;
				}
				self.invalidate();
			}
			Some(ActiveKind::Group) => {
				let mut actions = Vec::new();
				let key = if delta > 0 { VK_UP } else { VK_DOWN };
				if let Some(group) = self.active_group_mut() {
					for _ in 0..delta.abs() {
						actions.extend(group.on_key(key, false, false));
					}
				}
				self.handle_group_actions(actions);
				self.invalidate();
			}
			None => {}
		}
	}

	fn on_keydown(&mut self, vk: u32, _lparam: LPARAM) -> bool {
		let ctrl = unsafe { GetKeyState(VK_CONTROL) } < 0;
		let alt = unsafe { GetKeyState(VK_MENU) } < 0;
		let shift = unsafe { GetKeyState(VK_SHIFT) } < 0;

		if ctrl && shift {
			match vk {
				VK_G => {
					self.new_even_tab();
					return true;
				}
				VK_O => {
					self.new_opencode_tab();
					return true;
				}
				VK_T => {
					self.new_tab();
					return true;
				}
				VK_W => {
					self.close_active();
					return true;
				}
				VK_D => {
					self.split(Dir::Columns);
					return true;
				}
				VK_E => {
					self.split(Dir::Rows);
					return true;
				}
				VK_F => {
					self.open_file_dialog();
					return true;
				}
				_ => {}
			}
		}
		if ctrl && vk == VK_TAB {
			self.switch_tab(if shift { -1 } else { 1 });
			return true;
		}
		if alt && shift && (vk == VK_OEM_PLUS || vk == VK_ADD) {
			self.split(Dir::Columns);
			return true;
		}
		if alt && shift && (vk == VK_OEM_MINUS || vk == VK_SUBTRACT) {
			self.split(Dir::Rows);
			return true;
		}
		if !ctrl && !alt && shift && matches!(vk, VK_PRIOR | VK_NEXT) {
			self.scroll_active(if vk == VK_PRIOR { 5 } else { -5 });
			return true;
		}
		if alt && !ctrl {
			let direction = match vk {
				VK_LEFT => Some((-1, 0)),
				VK_RIGHT => Some((1, 0)),
				VK_UP => Some((0, -1)),
				VK_DOWN => Some((0, 1)),
				_ => None,
			};
			if let Some((dx, dy)) = direction {
				self.focus_dir(dx, dy);
				return true;
			}
		}
		if alt && !ctrl && (vk == VK_F4 || vk == VK_SPACE) {
			return false;
		}
		if ctrl && !alt && vk == VK_V {
			self.paste();
			return true;
		}
		if ctrl && !alt
			&& vk == VK_S
			&& matches!(self.active_kind(), Some(ActiveKind::Editor))
		{
			let saved = self.active_editor_mut().and_then(|editor| {
				let path = editor.path.to_string_lossy().into_owned();
				match editor.save() {
					Ok(()) => Some(path),
					Err(err) => {
						editor.status = err;
						None
					}
				}
			});
			if let Some(path) = saved {
				let window = self.active_pane().map(|pane| pane.id).unwrap_or(0);
				self.record(Event::FileSaved { window, path });
			}
			self.invalidate();
			return true;
		}

		match self.active_kind() {
			Some(ActiveKind::Editor) => self.editor_key(vk, ctrl, shift),
			Some(ActiveKind::Group) => self.group_key(vk, ctrl, shift),
			Some(ActiveKind::Terminal) => self.terminal_key(vk, ctrl, shift),
			None => false,
		}
	}

	fn editor_key(&mut self, vk: u32, ctrl: bool, shift: bool) -> bool {
		let page_rows = self
			.active_rect()
			.map(|rect| self.rows_for(rect).saturating_sub(1).max(1))
			.unwrap_or(1);
		match self.active_editor_mut() {
			Some(editor) => {
				let handled = editor.on_key(vk, ctrl, shift, page_rows);
				if handled {
					self.invalidate();
				}
				handled
			}
			None => false,
		}
	}

	fn group_key(&mut self, vk: u32, ctrl: bool, shift: bool) -> bool {
		let actions = match self.active_group_mut() {
			Some(group) => group.on_key(vk, ctrl, shift),
			None => return false,
		};
		self.handle_group_actions(actions);
		self.invalidate();
		true
	}

	fn terminal_key(&mut self, vk: u32, ctrl: bool, _shift: bool) -> bool {
		if ctrl {
			let byte = match vk {
				0x41..=0x5a => Some((vk - 0x40) as u8),
				VK_SPACE => Some(0),
				VK_OEM_4 => Some(0x1b),
				VK_OEM_5 => Some(0x1c),
				VK_OEM_6 => Some(0x1d),
				VK_6 => Some(0x1e),
				VK_OEM_MINUS => Some(0x1f),
				VK_RETURN => Some(b'\r'),
				VK_BACK => Some(0x7f),
				VK_TAB => Some(b'\t'),
				VK_ESCAPE => Some(0x1b),
				_ => None,
			};
			if let Some(byte) = byte {
				self.write(&[byte]);
				return true;
			}
			let seq: Option<&[u8]> = match vk {
				VK_UP => Some(b"\x1b[1;5A"),
				VK_DOWN => Some(b"\x1b[1;5B"),
				VK_RIGHT => Some(b"\x1b[1;5C"),
				VK_LEFT => Some(b"\x1b[1;5D"),
				_ => None,
			};
			if let Some(seq) = seq {
				self.write(seq);
				return true;
			}
			return false;
		}

		let app_cursor = self
			.active_pane()
			.and_then(|pane| pane.term().and_then(|term| term.lock().ok().map(|term| term.app_cursor)))
			.unwrap_or(false);
		let shift = unsafe { GetKeyState(VK_SHIFT) } < 0;
		let seq: Option<&[u8]> = match vk {
			VK_UP => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
			VK_DOWN => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
			VK_RIGHT => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
			VK_LEFT => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
			VK_HOME => Some(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
			VK_END => Some(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
			VK_INSERT => Some(b"\x1b[2~"),
			VK_DELETE => Some(b"\x1b[3~"),
			VK_PRIOR => Some(b"\x1b[5~"),
			VK_NEXT => Some(b"\x1b[6~"),
			VK_F1 => Some(b"\x1bOP"),
			VK_F2 => Some(b"\x1bOQ"),
			VK_F3 => Some(b"\x1bOR"),
			VK_F4 => Some(b"\x1bOS"),
			VK_F5 => Some(b"\x1b[15~"),
			VK_F6 => Some(b"\x1b[17~"),
			VK_F7 => Some(b"\x1b[18~"),
			VK_F8 => Some(b"\x1b[19~"),
			VK_F9 => Some(b"\x1b[20~"),
			VK_F10 => Some(b"\x1b[21~"),
			VK_F11 => Some(b"\x1b[23~"),
			VK_F12 => Some(b"\x1b[24~"),
			VK_TAB if shift => Some(b"\x1b[Z"),
			VK_ESCAPE => Some(b"\x1b"),
			_ => None,
		};
		if let Some(seq) = seq {
			self.write(seq);
			return true;
		}
		false
	}

	fn on_char(&mut self, wparam: WPARAM) -> bool {
		let ctrl = unsafe { GetKeyState(VK_CONTROL) } < 0;
		let alt = unsafe { GetKeyState(VK_MENU) } < 0;
		let shift = unsafe { GetKeyState(VK_SHIFT) } < 0;
		if ctrl && !alt {
			return true;
		}
		let unit = wparam as u16;
		let ch = if (0xd800..=0xdbff).contains(&unit) {
			self.high_surrogate = Some(unit);
			return true;
		} else if (0xdc00..=0xdfff).contains(&unit) {
			let high = self.high_surrogate.take().unwrap_or(0xfffd);
			let code = 0x10000 + (((high as u32 - 0xd800) << 10) | (unit as u32 - 0xdc00));
			char::from_u32(code).unwrap_or('\u{fffd}')
		} else {
			self.high_surrogate = None;
			char::from_u32(unit as u32).unwrap_or('\u{fffd}')
		};

		if alt && shift && matches!(ch, '+' | '-' | '_') {
			return true;
		}
		match self.active_kind() {
			Some(ActiveKind::Terminal) => {
				if shift && ch == '\t' {
					return true;
				}
				self.terminal_char(ch, alt);
				true
			}
			Some(ActiveKind::Editor) => {
				if alt {
					return true;
				}
				if (ch as u32) >= 0x20 && ch != '\u{7f}' {
					if let Some(editor) = self.active_editor_mut() {
						editor.insert_char(ch);
					}
					self.invalidate();
				}
				true
			}
			Some(ActiveKind::Group) => {
				if alt {
					return true;
				}
				if (ch as u32) >= 0x20 && ch != '\u{7f}' {
					if let Some(group) = self.active_group_mut() {
						group.insert_str(&ch.to_string());
					}
					self.invalidate();
				}
				true
			}
			None => false,
		}
	}

	fn terminal_char(&mut self, ch: char, alt: bool) {
		if ch == '\u{1b}' {
			return;
		}
		if ch == '\r' {
			self.write(&[b'\r']);
			return;
		}
		if ch == '\n' {
			self.write(&[b'\n']);
			return;
		}
		if ch == '\t' {
			self.write(&[b'\t']);
			return;
		}
		if ch == '\u{8}' {
			self.write(&[0x7f]);
			return;
		}
		if (ch as u32) < 0x20 || ch == '\u{7f}' {
			return;
		}

		let mut bytes = Vec::new();
		if alt {
			bytes.push(0x1b);
		}
		let mut buf = [0u8; 4];
		let s = ch.encode_utf8(&mut buf);
		bytes.extend_from_slice(s.as_bytes());
		self.write(&bytes);
	}
}

impl Drop for App {
	fn drop(&mut self) {
		self.tabs.clear();
		unsafe {
			if !self.mem_dc.is_null() {
				SelectObject(self.mem_dc, self.old_bmp);
				DeleteObject(self.mem_bmp);
				DeleteDC(self.mem_dc);
			}
			if !self.font.is_null() {
				DeleteObject(self.font);
			}
			if !self.default_brush.is_null() {
				DeleteObject(self.default_brush);
			}
		}
	}
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
	unsafe {
		if msg == WM_NCCREATE {
			let cs = lparam as *const CREATESTRUCTW;
			if !cs.is_null() {
				let app = (*cs).lpCreateParams as *mut App;
				SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
			}
			return DefWindowProcW(hwnd, msg, wparam, lparam);
		}

		let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
		if app.is_null() {
			return DefWindowProcW(hwnd, msg, wparam, lparam);
		}
		let app = &mut *app;

		match msg {
			WM_SIZE => {
				if !app.hwnd.is_null() {
					app.resize_backbuffer(hwnd);
					app.resize_panes();
					InvalidateRect(hwnd, null(), 0);
				}
				0
			}
			WM_PAINT => {
				if !app.hwnd.is_null() {
					app.paint(hwnd);
				}
				0
			}
			WM_KEYDOWN | WM_SYSKEYDOWN => {
				if app.on_keydown(wparam as u32, lparam) {
					0
				} else {
					DefWindowProcW(hwnd, msg, wparam, lparam)
				}
			}
			WM_CHAR | WM_SYSCHAR => {
				if app.on_char(wparam) {
					0
				} else {
					DefWindowProcW(hwnd, msg, wparam, lparam)
				}
			}
			WM_LBUTTONDOWN => {
				let x = (lparam & 0xffff) as i16 as i32;
				let y = ((lparam >> 16) & 0xffff) as i16 as i32;
				app.click_at(x, y);
				0
			}
			WM_MOUSEWHEEL => {
				let delta = (((wparam >> 16) & 0xffff) as i16 as i32) / 120;
				let mut point = POINT {
					x: (lparam & 0xffff) as i16 as i32,
					y: ((lparam >> 16) & 0xffff) as i16 as i32,
				};
				ScreenToClient(hwnd, &mut point);
				app.wheel_at(point.x, point.y, delta);
				0
			}
			WM_IME_STARTCOMPOSITION => {
				app.ime_position(hwnd);
				0
			}
			WM_IME_COMPOSITION => {
				app.on_ime_composition(hwnd, lparam);
				0
			}
			WM_IME_ENDCOMPOSITION => 0,
			WM_APP_REFRESH => {
				InvalidateRect(hwnd, null(), 0);
				0
			}
			WM_APP_PANE_EXIT => {
				app.close_pane(wparam as u64);
				0
			}
			WM_APP_TICK => {
				app.tick();
				0
			}
			crate::solid::WM_APP_LSP => {
				app.tick();
				0
			}
			WM_ERASEBKGND => 1,
			WM_CLOSE => {
				DestroyWindow(hwnd);
				0
			}
			WM_DESTROY => {
				PostQuitMessage(0);
				0
			}
			_ => DefWindowProcW(hwnd, msg, wparam, lparam),
		}
	}
}

pub fn run(
	shell: Option<String>,
	image_path: Option<String>,
	initial_file: Option<String>,
) -> io::Result<()> {
	let label = shell_label(shell.as_deref());
	let title_text = format!("Delta - {label}");
	let initial = initial_file.clone();
	let app = Box::new(App::new(shell, image_path)?);
	let app_ptr = Box::into_raw(app);

	unsafe {
		let hinstance = GetModuleHandleW(null());
		let class = wide("DeltaTermWindow");
		let title = wide(&title_text);
		let wc = WNDCLASSW {
			style: CS_HREDRAW | CS_VREDRAW,
			lpfnWndProc: Some(wndproc),
			cbClsExtra: 0,
			cbWndExtra: 0,
			hInstance: hinstance,
			hIcon: null_mut(),
			hCursor: LoadCursorW(null_mut(), IDC_ARROW),
			hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
			lpszMenuName: null(),
			lpszClassName: class.as_ptr(),
		};
		if RegisterClassW(&wc) == 0 {
			drop(Box::from_raw(app_ptr));
			return Err(io::Error::last_os_error());
		}

		let mut work: RECT = zeroed();
		SystemParametersInfoW(
			SPI_GETWORKAREA,
			0,
			&mut work as *mut RECT as *mut c_void,
			0,
		);
		let work_w = (work.right - work.left).max(320) as i32;
		let work_h = (work.bottom - work.top).max(240) as i32;
		let mut rect = RECT {
			left: 0,
			top: 0,
			right: 1200.min(work_w),
			bottom: 760.min(work_h - 16),
		};
		AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, 0);
		let width = rect.right - rect.left;
		let height = rect.bottom - rect.top;
		let x = work.left + ((work_w - width) / 2).max(0);
		let y = work.top + ((work_h - height) / 2).max(0);
		let hwnd = CreateWindowExW(
			0,
			class.as_ptr(),
			title.as_ptr(),
			WS_OVERLAPPEDWINDOW | WS_VISIBLE,
			x,
			y,
			width,
			height,
			null_mut(),
			null_mut(),
			hinstance,
			app_ptr as *const c_void,
		);
		if hwnd.is_null() {
			drop(Box::from_raw(app_ptr));
			return Err(io::Error::last_os_error());
		}

		(*app_ptr).hwnd = hwnd;
		SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
		(*app_ptr).init_gdi(hwnd);
		(*app_ptr).resize_panes();
		(*app_ptr).start_pane_reader(1);
		(*app_ptr).load_initial_image();
		if let Some(file) = initial {
			(*app_ptr).open_editor_path(file);
		}
		let tick_hwnd = hwnd as usize;
		thread::spawn(move || loop {
			thread::sleep(Duration::from_millis(250));
			let posted = PostMessageW(tick_hwnd as HWND, WM_APP_TICK, 0, 0);
			if posted == 0 {
				break;
			}
		});
		ShowWindow(hwnd, SW_SHOW);
		SetForegroundWindow(hwnd);
		UpdateWindow(hwnd);

		let mut msg: MSG = zeroed();
		while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
		drop(Box::from_raw(app_ptr));
	}
	Ok(())
}

fn shell_label(shell: Option<&str>) -> String {
	match shell {
		None => String::from("odd"),
		Some("cmd") => String::from("cmd"),
		Some("powershell") => String::from("powershell"),
		Some("pwsh") => String::from("pwsh"),
		Some(other) => other.to_string(),
	}
}

fn odd_path() -> Option<PathBuf> {
	let exe = std::env::current_exe().ok()?;
	let candidate = exe.parent()?.join("delta-odd.exe");
	if candidate.is_file() {
		Some(candidate)
	} else {
		None
	}
}

fn cmd_command() -> String {
	let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| String::from("cmd.exe"));
	format!("\"{}\" /K chcp 65001>nul", comspec)
}

fn command_for(shell: Option<String>) -> String {
	match shell.as_deref() {
		None => match odd_path() {
			Some(path) => quote_path(&path.to_string_lossy()),
			None => cmd_command(),
		},
		Some("cmd") => cmd_command(),
		Some("powershell") => String::from("powershell.exe -NoLogo -NoExit"),
		Some("pwsh") => String::from("pwsh.exe -NoLogo -NoExit"),
		Some(other) => other.to_string(),
	}
}

fn opencode_path() -> String {
	std::env::var("DELTA_OPENCODE").unwrap_or_else(|_| String::from(r"D:\OPENCODE\opencode.exe"))
}

fn quote_path(path: &str) -> String {
	if path.contains(' ') {
		format!("\"{path}\"")
	} else {
		String::from(path)
	}
}

fn group_left(cols: usize) -> usize {
	24usize.min(cols / 3).max(12)
}

fn gutter_cols(editor: &Editor) -> usize {
	editor.buffer.lines.len().max(1).to_string().len() + 2
}

fn display_width(ch: char) -> usize {
	UnicodeWidthChar::width(ch).unwrap_or(1).max(1)
}

fn display_width_str(text: &str) -> usize {
	text.chars().map(display_width).sum()
}

fn cells_width(line: &[char], upto: usize) -> usize {
	let mut used = 0;
	for &ch in line.iter().take(upto) {
		used += if ch == '\t' {
			4 - (used % 4)
		} else {
			display_width(ch)
		};
	}
	used
}

fn char_at_cells(line: &[char], cells: usize) -> usize {
	let mut used = 0;
	for (index, &ch) in line.iter().enumerate() {
		let step = if ch == '\t' {
			4 - (used % 4)
		} else {
			display_width(ch)
		};
		if used + step > cells {
			return index;
		}
		used += step;
	}
	line.len()
}

fn truncate_text(text: &str, width: usize) -> String {
	let mut out = String::new();
	let mut used = 0;
	for ch in text.chars() {
		if ch == '\n' || ch == '\r' {
			break;
		}
		let step = if ch == '\t' { 4 } else { display_width(ch) };
		if used + step > width {
			break;
		}
		out.push(ch);
		used += step;
	}
	out
}

fn clipboard_text() -> Option<String> {
	unsafe {
		if OpenClipboard(null_mut()) == 0 {
			return None;
		}
		let handle = GetClipboardData(CF_UNICODETEXT);
		let mut out = None;
		if !handle.is_null() {
			let ptr = GlobalLock(handle) as *const u16;
			if !ptr.is_null() {
				let mut len = 0usize;
				while *ptr.add(len) != 0 {
					len += 1;
				}
				out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(
					ptr, len,
				)));
				GlobalUnlock(handle);
			}
		}
		CloseClipboard();
		out
	}
}

fn clipboard_image_path() -> Option<String> {
	let dib = clipboard_dib()?;
	let png = dib_to_png(&dib)?;
	let stamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_millis())
		.unwrap_or(0);
	let path = std::env::temp_dir().join(format!("delta-paste-{stamp}.png"));
	std::fs::write(&path, png).ok()?;
	Some(path.to_string_lossy().into_owned())
}

fn clipboard_dib() -> Option<Vec<u8>> {
	unsafe {
		if OpenClipboard(null_mut()) == 0 {
			return None;
		}
		let handle = GetClipboardData(CF_DIB);
		let mut out = None;
		if !handle.is_null() {
			let ptr = GlobalLock(handle) as *const u8;
			if !ptr.is_null() {
				let header = std::slice::from_raw_parts(ptr, 40);
				let read32 = |at: usize| -> u32 {
					u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
				};
				let read16 = |at: usize| -> u16 {
					u16::from_le_bytes([header[at], header[at + 1]])
				};
				let offset = read32(0) as usize;
				let width = read32(4) as i32;
				let height = read32(8) as i32;
				let bpp = read16(14) as usize;
				if bpp == 24 || bpp == 32 {
					let stride = ((width.max(0) as usize * bpp + 31) / 32) * 4;
					let rows = height.unsigned_abs() as usize;
					let len = offset + stride * rows;
					let data = std::slice::from_raw_parts(ptr, len);
					out = Some(data.to_vec());
				}
				GlobalUnlock(handle);
			}
		}
		CloseClipboard();
		out
	}
}

fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
	if dib.len() < 40 {
		return None;
	}
	let read32 = |at: usize| -> u32 {
		u32::from_le_bytes([dib[at], dib[at + 1], dib[at + 2], dib[at + 3]])
	};
	let read16 = |at: usize| -> u16 { u16::from_le_bytes([dib[at], dib[at + 1]]) };
	let offset = read32(0) as usize;
	let width = read32(4) as i32;
	let signed_height = read32(8) as i32;
	let bpp = read16(14) as usize;
	let compression = read32(16);
	if width <= 0 || signed_height == 0 || (bpp != 24 && bpp != 32) {
		return None;
	}
	let mask_bytes = match compression {
		0 => 0,
		3 if bpp == 32 => 12,
		_ => return None,
	};
	let width = width as usize;
	let top_down = signed_height < 0;
	let height = signed_height.unsigned_abs() as usize;
	let stride = ((width * bpp + 31) / 32) * 4;
	let pixel_bytes = stride * height;
	let mut start = offset + mask_bytes;
	if start + pixel_bytes > dib.len() {
		start = dib.len().checked_sub(pixel_bytes)?;
	}
	if start < offset || start + pixel_bytes > dib.len() {
		return None;
	}
	let offset = start;
	let mut rgba = vec![0u8; width * height * 4];
	let bytes = bpp / 8;
	let mut opaque = false;
	for row in 0..height {
		let source = if top_down { row } else { height - 1 - row };
		let line = offset + source * stride;
		for column in 0..width {
			let pixel = line + column * bytes;
			let alpha = if bytes == 4 { dib[pixel + 3] } else { 255 };
			if alpha != 0 {
				opaque = true;
			}
			let target = (row * width + column) * 4;
			rgba[target] = dib[pixel + 2];
			rgba[target + 1] = dib[pixel + 1];
			rgba[target + 2] = dib[pixel];
			rgba[target + 3] = alpha;
		}
	}
	if !opaque {
		for pixel in rgba.chunks_exact_mut(4) {
			pixel[3] = 255;
		}
	}
	let mut png = Vec::new();
	let mut encoder = png::Encoder::new(&mut png, width as u32, height as u32);
	encoder.set_color(png::ColorType::Rgba);
	encoder.set_depth(png::BitDepth::Eight);
	let mut writer = encoder.write_header().ok()?;
	writer.write_image_data(&rgba).ok()?;
	writer.finish().ok()?;
	Some(png)
}

fn pick_file(hwnd: HWND) -> Option<String> {
	let mut buffer = vec![0u16; 32768];
	let filter: Vec<u16> = "All files\0*.*\0Text\0*.txt\0Rust\0*.rs\0C\0*.c;*.h\0Python\0*.py\0JSON\0*.json\0"
		.encode_utf16()
		.chain(std::iter::once(0))
		.collect();
	let title = wide("Open file");
	let mut ofn: OPENFILENAMEW = unsafe { zeroed() };
	ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
	ofn.hwndOwner = hwnd;
	ofn.lpstrFilter = filter.as_ptr();
	ofn.nFilterIndex = 1;
	ofn.lpstrFile = buffer.as_mut_ptr();
	ofn.nMaxFile = buffer.len() as u32;
	ofn.lpstrTitle = title.as_ptr();
	ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
	let ok = unsafe { GetOpenFileNameW(&mut ofn) };
	if ok == 0 {
		return None;
	}
	let len = buffer.iter().position(|&ch| ch == 0).unwrap_or(0);
	if len == 0 {
		return None;
	}
	Some(String::from_utf16_lossy(&buffer[..len]))
}

fn wide(text: &str) -> Vec<u16> {
	text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
	(r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

fn indexed(n: u8, bold: bool) -> COLORREF {
	const PALETTE: [(u8, u8, u8); 16] = [
		(0, 0, 0),
		(128, 0, 0),
		(0, 128, 0),
		(128, 128, 0),
		(0, 0, 128),
		(128, 0, 128),
		(0, 128, 128),
		(192, 192, 192),
		(128, 128, 128),
		(255, 0, 0),
		(0, 255, 0),
		(255, 255, 0),
		(0, 0, 255),
		(255, 0, 255),
		(0, 255, 255),
		(255, 255, 255),
	];
	let n = if bold && n < 8 { n + 8 } else { n };
	if n < 16 {
		let (r, g, b) = PALETTE[n as usize];
		rgb(r, g, b)
	} else if n < 232 {
		let v = n - 16;
		let r = v / 36;
		let g = (v % 36) / 6;
		let b = v % 6;
		let step = |x: u8| -> u8 { if x == 0 { 0 } else { 55 + x * 40 } };
		rgb(step(r), step(g), step(b))
	} else {
		let v = 8 + (n - 232) * 10;
		rgb(v, v, v)
	}
}

fn colorref(color: Color, background: bool, bold: bool) -> COLORREF {
	match color {
		Color::Default => {
			if background {
				rgb(0, 0, 0)
			} else if bold {
				rgb(255, 255, 255)
			} else {
				rgb(220, 220, 220)
			}
		}
		Color::Indexed(n) => indexed(n, bold && !background),
		Color::Rgb(r, g, b) => rgb(r, g, b),
	}
}

unsafe fn ime_string(himc: HIMC, flag: u32) -> String {
	let len = unsafe { ImmGetCompositionStringW(himc, flag, null_mut(), 0) };
	if len <= 0 {
		return String::new();
	}
	let mut buf = vec![0u16; (len as usize + 1) / 2];
	let n = unsafe {
		ImmGetCompositionStringW(himc, flag, buf.as_mut_ptr() as *mut c_void, len as u32)
	};
	if n <= 0 {
		return String::new();
	}
	String::from_utf16_lossy(&buf[..(n as usize) / 2])
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dib_converts_to_png_top_down_and_bottom_up() {
		let width = 2usize;
		let height = 2usize;
		let stride = 8usize;
		let mut dib = vec![0u8; 40 + stride * height];
		dib[0..4].copy_from_slice(&40u32.to_le_bytes());
		dib[4..8].copy_from_slice(&(width as u32).to_le_bytes());
		dib[8..12].copy_from_slice(&(height as i32).to_le_bytes());
		dib[14..16].copy_from_slice(&24u16.to_le_bytes());
		for column in 0..width {
			let at = 40 + column * 3;
			dib[at] = 255;
		}
		for column in 0..width {
			let at = 40 + stride + column * 3;
			dib[at + 2] = 255;
		}
		let png = dib_to_png(&dib).expect("png");
		assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
		let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
		let mut reader = decoder.read_info().unwrap();
		let mut buffer = vec![0u8; reader.output_buffer_size()];
		let info = reader.next_frame(&mut buffer).unwrap();
		assert_eq!((info.width, info.height), (2, 2));
		assert_eq!(&buffer[..4], &[255, 0, 0, 255]);
	}

	#[test]
	fn dib_bitfields_converts_to_png() {
		let width = 2usize;
		let height = 2usize;
		let stride = 8usize;
		let mut dib = vec![0u8; 52 + stride * height];
		dib[0..4].copy_from_slice(&40u32.to_le_bytes());
		dib[4..8].copy_from_slice(&(width as u32).to_le_bytes());
		dib[8..12].copy_from_slice(&(height as i32).to_le_bytes());
		dib[14..16].copy_from_slice(&32u16.to_le_bytes());
		dib[16..20].copy_from_slice(&3u32.to_le_bytes());
		for column in 0..width {
			let top = 52 + column * 4;
			dib[top + 2] = 255;
			dib[top + 3] = 255;
			let bottom = 52 + stride + column * 4;
			dib[bottom + 1] = 255;
			dib[bottom + 3] = 255;
		}
		let png = dib_to_png(&dib).expect("png");
		let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
		let mut reader = decoder.read_info().unwrap();
		let mut buffer = vec![0u8; reader.output_buffer_size()];
		reader.next_frame(&mut buffer).unwrap();
		assert_eq!(&buffer[..4], &[0, 255, 0, 255]);
	}
}
