//! Delta window: one Win32 window, tmux-style tabs and panes, ownership and
//! takeover, key/mouse/IME input, MCP dispatch. The byte layer lives in
//! delta-vt; this module owns the UI thread.

use std::collections::HashMap;
use std::mem::zeroed;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use delta_vt::{MouseEncoding, MouseMode};
use serde_json::{Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
	BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient, UpdateWindow,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
	GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
	OPENFILENAMEW,
};
use windows_sys::Win32::UI::HiDpi::{
	DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::Ime::{
	CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR,
	ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow,
	ImmSetCompositionWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
	GetKeyState, GetKeyboardLayout, GetKeyboardState, ReleaseCapture, SetCapture, ToUnicodeEx,
	VK_CAPITAL, VK_CONTROL, VK_ESCAPE, VK_F1, VK_MENU, VK_NUMLOCK, VK_RCONTROL, VK_RMENU, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
	CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
	DestroyWindow, DispatchMessageW, FindWindowW, GWLP_USERDATA, GetClientRect,
	GetForegroundWindow, GetMessageW, GetWindowLongPtrW, IDC_ARROW, LoadCursorW, MSG,
	PostQuitMessage, RegisterClassW, SW_SHOW, SendMessageW, SetTimer, SetWindowLongPtrW,
	ShowWindow, TranslateMessage, UnregisterClassW, WM_APP, WM_CHAR, WM_CLOSE, WM_COPYDATA,
	WM_DESTROY, WM_ERASEBKGND, WM_IME_CHAR, WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION,
	WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
	WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETFOCUS, WM_SIZE, WM_SYSCHAR,
	WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::canvas::{self, Canvas};
use crate::clipboard;
use crate::even::{EvenEvent, GroupPane};
use crate::input::Key;
use crate::layout::{Dir, Layout, LayoutType, PaneId, Rect};
use crate::mcp::{self, McpServer};
use crate::opencode::Client;
use crate::solid::Editor;
use crate::term::TerminalPane;

const WM_DELTA_DATA: u32 = WM_APP + 10;
const WM_DELTA_EXIT: u32 = WM_APP + 11;
const COPYDATA_TAG: usize = 0x4445_4C54;
const TIMER_ID: usize = 1;
/// The `p` popup keeps this many window rows; the wheel scrolls the rest.
const PANE_LIST_ROWS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
	Normal,
	Insert,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Owner {
	Normal,
	Agent(String),
	Share(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventKind {
	AiTask,
	/// An AI edit of the file behind a solid window. The write itself is
	/// synchronous; the event lingers briefly so the human sees the file
	/// changed and the window cannot close under it.
	AiEdit,
}

/// How long an AI edit event stays bound to its window after the write.
const AI_EDIT_EVENT_TTL: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct EventRecord {
	id: u64,
	pane: PaneId,
	kind: EventKind,
	label: String,
	member: Option<String>,
	expires: Option<Instant>,
}

enum Body {
	Term(TerminalPane),
	Solid(Editor),
	Even(Box<GroupPane>),
}

struct Pane {
	id: PaneId,
	body: Body,
	owner: Owner,
	seen: bool,
	ai_created: bool,
	creator: Option<String>,
	title: Option<String>,
	group_id: u64,
	member: Option<String>,
	/// Point size of this window's own font, changed by Ctrl+= / Ctrl+-.
	font: i32,
	/// Cell size that goes with `font`, as measured by the canvas.
	cell: (i32, i32),
}

struct Tab {
	layout: Layout,
	/// The layout a zoom replaced, as in tmux window_zoom; zoom is on while
	/// this holds the tiled layout.
	saved: Option<Layout>,
	panes: HashMap<PaneId, Pane>,
	focus: PaneId,
	/// Windows that live in this tab but are not in its layout, so they keep
	/// running without being drawn. Only a close (or a raised limit) brings
	/// one back; see `Priority` for the order.
	hidden: Vec<PaneId>,
}

/// When a window was last switched to. The human's switch time comes first;
/// equal times fall back to the AI's, then to the window number.
#[derive(Clone, Copy, Default, Debug)]
struct Priority {
	human: u64,
	ai: u64,
}

enum Modal {
	Help,
	Events { pane: PaneId },
	Takeover { pane: PaneId, agent: String },
	Tabs,
	CloseAgent { pane: PaneId, agent: String },
	PaneLimit { scroll: isize },
	Notice(String),
}

struct Drag {
	pane: PaneId,
	moved: bool,
}

pub struct App {
	hwnd: HWND,
	canvas: Canvas,
	width: i32,
	height: i32,
	tabs: Vec<Tab>,
	active: usize,
	next_pane: PaneId,
	next_event: u64,
	next_group: u64,
	mode: Mode,
	scroll_mode: bool,
	/// Wheel deltas arrive in fractions of 120 on high-resolution mice; they
	/// accumulate here until a whole notch is worth acting on.
	wheel_accum: i32,
	show_numbers: bool,
	modal: Option<Modal>,
	events: Vec<EventRecord>,
	auth: HashMap<(PaneId, String), ()>,
	server: Option<crate::opencode::Server>,
	client: Option<Client>,
	mcp: Option<McpServer>,
	pending_takeovers: Vec<mcp::McpRequest>,
	child_env: Vec<(String, String)>,
	shell: String,
	/// How many windows a tab draws at once; set through the `p` popup.
	pane_limit: usize,
	clock: u64,
	prio: HashMap<PaneId, Priority>,
	drag: Option<Drag>,
	key_chars: HashMap<u16, char>,
	ime_text: String,
	ime_caret: Option<(i32, i32)>,
	quit: bool,
}

pub fn run(shell: Option<String>, file: Option<PathBuf>) -> i32 {
	unsafe {
		let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
		let module = GetModuleHandleW(null());
		let class_name = canvas::wide("DeltaWindowClass");
		if let Some(path) = &file {
			// A running window owns this session; hand the file to it instead of
			// opening a second copy of the same editor.
			let existing = FindWindowW(class_name.as_ptr(), null());
			if !existing.is_null() && forward_open(existing, path) {
				return 0;
			}
		}
		let cursor = LoadCursorW(null_mut(), IDC_ARROW);
		let wc = WNDCLASSW {
			style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
			lpfnWndProc: Some(wndproc),
			cbClsExtra: 0,
			cbWndExtra: 0,
			hInstance: module,
			hIcon: null_mut(),
			hCursor: cursor,
			hbrBackground: null_mut(),
			lpszMenuName: null(),
			lpszClassName: class_name.as_ptr(),
		};
		if RegisterClassW(&wc) == 0 {
			return 1;
		}
		let title = canvas::wide("Delta");
		let hwnd = CreateWindowExW(
			0,
			class_name.as_ptr(),
			title.as_ptr(),
			WS_OVERLAPPEDWINDOW | WS_VISIBLE,
			CW_USEDEFAULT,
			CW_USEDEFAULT,
			1180,
			720,
			null_mut(),
			null_mut(),
			module,
			null_mut(),
		);
		if hwnd.is_null() {
			UnregisterClassW(class_name.as_ptr(), module);
			return 1;
		}
		let mut app = match App::new(hwnd, shell, file) {
			Ok(app) => Box::new(app),
			Err(err) => {
				eprintln!("delta: {err}");
				DestroyWindow(hwnd);
				UnregisterClassW(class_name.as_ptr(), module);
				return 1;
			}
		};
		SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *app) as *mut App as isize);
		ShowWindow(hwnd, SW_SHOW);
		UpdateWindow(hwnd);
		SetTimer(hwnd, TIMER_ID, 16, None);

		let mut msg: MSG = zeroed();
		while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
			// IME 只有在消息循环里经过 TranslateMessage 才会处理按键，缺这一步
			// 中文输入法不会出候选、也不会提交汉字。
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
		SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
		drop(app);
		UnregisterClassW(class_name.as_ptr(), module);
		0
	}
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
	let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
	if ptr.is_null() {
		return DefWindowProcW(hwnd, msg, wparam, lparam);
	}
	(*ptr).handle(hwnd, msg, wparam, lparam)
}

impl App {
	fn new(hwnd: HWND, shell: Option<String>, file: Option<PathBuf>) -> Result<Self, String> {
		let mut canvas = Canvas::new(hwnd).ok_or_else(|| String::from("GDI 初始化失败"))?;
		let mut client: RECT = unsafe { zeroed() };
		unsafe {
			GetClientRect(hwnd, &mut client);
		}
		let (width, height) = (client.right - client.left, client.bottom - client.top);
		canvas.resize(hwnd, width, height);
		let mcp = mcp::start().ok();
		let server = crate::opencode::Server::start().ok();
		let client = server.as_ref().map(|s| {
			let mut client = s.client.clone();
			client.mcp_url = mcp.as_ref().map(|m| m.url.clone());
			client
		});
		let mut child_env = server.as_ref().map(|s| s.env()).unwrap_or_default();
		if let Some(mcp) = &mcp {
			child_env.push((String::from("DELTA_MCP_URL"), mcp.url.clone()));
		}
		if let Ok(exe) = std::env::current_exe() {
			if let Some(dir) = exe.parent() {
				let mut paths = vec![dir.to_path_buf()];
				paths.extend(std::env::split_paths(
					&std::env::var_os("PATH").unwrap_or_default(),
				));
				if let Ok(path) = std::env::join_paths(paths) {
					child_env.push((String::from("PATH"), path.to_string_lossy().into_owned()));
				}
			}
			let exe = exe.to_string_lossy().into_owned();
			child_env.push((String::from("DELTA_EXE"), exe.clone()));
			child_env.push((String::from("SOLID"), exe));
		}
		let shell = shell
			.or_else(|| std::env::var("DELTA_SHELL").ok())
			.unwrap_or_else(default_shell);
		let mut app = Self {
			hwnd,
			canvas,
			width,
			height,
			tabs: Vec::new(),
			active: 0,
			next_pane: 1,
			next_event: 1,
			next_group: 1,
			mode: Mode::Insert,
			scroll_mode: false,
			wheel_accum: 0,
			show_numbers: false,
			modal: None,
			events: Vec::new(),
			auth: HashMap::new(),
			server,
			client,
			mcp,
			pending_takeovers: Vec::new(),
			child_env,
			shell,
			pane_limit: 4,
			clock: 0,
			prio: HashMap::new(),
			drag: None,
			key_chars: HashMap::new(),
			ime_text: String::new(),
			ime_caret: None,
			quit: false,
		};
		if let Some(path) = file {
			app.open_editor(path);
		}
		if app.tabs.is_empty() {
			app.new_terminal_tab();
		}
		Ok(app)
	}

	fn handle(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
		match msg {
			WM_PAINT => {
				let mut ps: PAINTSTRUCT = unsafe { zeroed() };
				unsafe {
					BeginPaint(hwnd, &mut ps);
				}
				self.paint();
				unsafe {
					EndPaint(hwnd, &ps);
				}
				0
			}
			WM_ERASEBKGND => 1,
			WM_SIZE => {
				self.width = (lparam & 0xffff) as i32;
				self.height = ((lparam >> 16) & 0xffff) as i32;
				self.canvas.resize(hwnd, self.width, self.height);
				self.resize_panes();
				self.paint();
				0
			}
			WM_TIMER => {
				if wparam == TIMER_ID {
					self.tick();
				}
				0
			}
			WM_COPYDATA => {
				self.import_path(lparam);
				1
			}
			WM_DELTA_DATA | WM_DELTA_EXIT => {
				if msg == WM_DELTA_EXIT {
					// The reader posts the pane id in wParam; the tick below also notices
					// the same exit through the process handle.
					self.term_exit(wparam as PaneId);
				}
				self.tick();
				0
			}
			WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
				let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
				let mut key = make_key(wparam as u16, lparam, down);
				if down {
					if let Some(ch) = key.ch.filter(|ch| !ch.is_control()) {
						self.key_chars.insert(key.vk, ch);
					}
				} else if key.ch.is_none() {
					key.ch = self.key_chars.get(&key.vk).copied();
				}
				if !self.key(&key, msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP) {
					return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
				}
				0
			}
			WM_CHAR | WM_SYSCHAR | WM_IME_CHAR => 0,
			WM_IME_STARTCOMPOSITION => {
				self.ime_position(hwnd);
				0
			}
			WM_IME_COMPOSITION => {
				self.ime_composition(hwnd, lparam);
				0
			}
			WM_IME_ENDCOMPOSITION => {
				self.ime_text.clear();
				0
			}
			WM_SETFOCUS => {
				self.forward_focus(true);
				0
			}
			WM_KILLFOCUS => {
				self.forward_focus(false);
				0
			}
			WM_LBUTTONDOWN => {
				let (x, y) = point_of(lparam);
				self.mouse_down(x, y);
				0
			}
			WM_LBUTTONUP => {
				let (x, y) = point_of(lparam);
				self.mouse_up(x, y);
				0
			}
			WM_LBUTTONDBLCLK => {
				let (x, y) = point_of(lparam);
				self.mouse_double(x, y);
				0
			}
			WM_MOUSEMOVE => {
				let (x, y) = point_of(lparam);
				self.mouse_move(x, y);
				0
			}
			WM_MOUSEWHEEL => {
				let raw = ((wparam >> 16) & 0xffff) as i16 as i32;
				if raw != 0 {
					// Drivers report high-resolution wheels in fractions of
					// WHEEL_DELTA; dropping those fractions makes smooth mice
					// look dead. Keep the remainder until a notch is whole.
					if raw.signum() != self.wheel_accum.signum() {
						self.wheel_accum = 0;
					}
					self.wheel_accum += raw;
				}
				let notches = self.wheel_accum / 120;
				if notches != 0 {
					self.wheel_accum -= notches * 120;
					let (sx, sy) = point_of(lparam);
					let mut point = POINT { x: sx, y: sy };
					unsafe {
						ScreenToClient(hwnd, &mut point);
					}
					let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) < 0 };
					if ctrl
						&& !self.scroll_mode
						&& self.mode == Mode::Normal
						&& self.modal.is_none()
					{
						if let Some(id) =
							self.mouse_target(point.x, point.y).or_else(|| self.focused_id())
						{
							self.zoom_font_at(id, notches * 2);
						}
					} else {
						self.mouse_wheel(point.x, point.y, notches);
					}
				}
				0
			}
			WM_CLOSE => {
				self.request_close_app();
				0
			}
			WM_DESTROY => {
				self.quit = true;
				unsafe {
					PostQuitMessage(0);
				}
				0
			}
			_ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
		}
	}

	fn tick(&mut self) {
		let ids: Vec<PaneId> = self
			.tabs
			.iter()
			.flat_map(|t| t.panes.keys().copied())
			.collect();
		let mut ended = Vec::new();
		let mut finished = Vec::new();
		for id in ids {
			if let Some(pane) = self.pane_mut(id) {
				if let Body::Term(term) = &mut pane.body {
					term.drain();
					if term.screen.command_finished.take().is_some() {
						finished.push(id);
					}
					if !term.exited && term.process_exited() {
						ended.push(id);
					}
				}
			}
		}
		for id in ended {
			self.term_exit(id);
		}
		for id in finished {
			self.events.retain(|event| event.pane != id);
			self.clear_owner_if_idle(id);
		}
		self.handle_even_events();
		self.sync_events();
		self.handle_mcp();
		self.mark_seen();
		self.sync_owner_to_mode();
		self.paint();
	}

	/// Leaving scroll mode goes back to the live screen, as in less or tmux;
	/// a frozen view with the mode off would look like a hung terminal.
	fn leave_scroll(&mut self) {
		self.scroll_mode = false;
		if let Some(pane) = self.focused_mut() {
			if let Body::Term(term) = &mut pane.body {
				term.scroll = 0;
			}
		}
	}

	/// A shell that ends on its own takes its pane with it, as in any terminal.
	/// Agent- and share-owned panes stay so their owner can still see what happened
	/// and close them through the usual rule.
	fn term_exit(&mut self, id: PaneId) {
		let normal = self
			.pane(id)
			.map(|pane| matches!(pane.owner, Owner::Normal))
			.unwrap_or(false);
		if normal && !self.events.iter().any(|e| e.pane == id) {
			self.remove_pane_direct(id);
			return;
		}
		if let Some(pane) = self.pane_mut(id) {
			if let Body::Term(term) = &mut pane.body {
				term.exited = true;
			}
		}
		self.events.retain(|e| e.pane != id);
		self.clear_owner_if_idle(id);
	}

	fn content_area(&self) -> Rect {
		Rect::new(0, 0, self.width, (self.height - self.canvas.cell_h).max(1))
	}

	/// The tiled area in terminal cells, which is what layout geometry counts.
	fn content_cells(&self) -> (u32, u32) {
		let area = self.content_area();
		let cw = self.canvas.cell_w.max(1);
		let ch = self.canvas.cell_h.max(1);
		(
			(area.width() / cw).max(1) as u32,
			(area.height() / ch).max(1) as u32,
		)
	}

	fn rects_of(&self, tab: &Tab, area: Rect) -> Vec<(PaneId, Rect)> {
		tab.layout
			.rects(area, self.canvas.cell_w.max(1), self.canvas.cell_h.max(1))
	}

	fn pane(&self, id: PaneId) -> Option<&Pane> {
		self.tabs.iter().find_map(|t| t.panes.get(&id))
	}

	fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
		self.tabs.iter_mut().find_map(|t| t.panes.get_mut(&id))
	}

	fn tab_index_of(&self, id: PaneId) -> Option<usize> {
		self.tabs.iter().position(|t| t.panes.contains_key(&id))
	}

	fn focused_id(&self) -> Option<PaneId> {
		self.tabs.get(self.active).map(|t| t.focus)
	}

	fn focused(&self) -> Option<&Pane> {
		let tab = self.tabs.get(self.active)?;
		tab.panes.get(&tab.focus)
	}

	fn focused_mut(&mut self) -> Option<&mut Pane> {
		let active = self.active;
		let tab = self.tabs.get_mut(active)?;
		let focus = tab.focus;
		tab.panes.get_mut(&focus)
	}

	fn pane_rect(&self, id: PaneId) -> Option<Rect> {
		let tab = self.tabs.get(self.active)?;
		if !tab.panes.contains_key(&id) {
			return None;
		}
		let area = self.content_area();
		self.rects_of(tab, area)
			.into_iter()
			.find(|(p, _)| *p == id)
			.map(|(_, r)| r)
	}

	fn mark_seen(&mut self) {
		if unsafe { GetForegroundWindow() } != self.hwnd {
			return;
		}
		let active = self.active;
		if let Some(tab) = self.tabs.get_mut(active) {
			let focus = tab.focus;
			if let Some(pane) = tab.panes.get_mut(&focus) {
				if pane.owner == Owner::Normal && self.mode != Mode::Normal {
					if !pane.seen {
						self.auth.retain(|(id, _), _| *id != focus);
					}
					pane.seen = true;
				}
			}
		}
	}

	/// Agent is the AI window's normal, share its insert. The keyboard mode is
	/// the human's choice and never depends on who holds the window; only the
	/// label and the permission layer follow the owner. So the owner is brought
	/// in line with the mode, never the other way round.
	fn sync_owner_to_mode(&mut self) {
		let mode = self.mode;
		let Some(pane) = self.focused_mut() else {
			return;
		};
		match pane.owner.clone() {
			Owner::Agent(agent) if mode == Mode::Insert => pane.owner = Owner::Share(agent),
			Owner::Share(agent) if mode == Mode::Normal => pane.owner = Owner::Agent(agent),
			_ => {}
		}
	}

	fn resize_panes(&mut self) {
		let area = self.content_area();
		let base_w = self.canvas.cell_w.max(1);
		let base_h = self.canvas.cell_h.max(1);
		let (cols, rows) = self.content_cells();
		let mut sizes: Vec<i32> = self
			.tabs
			.iter()
			.flat_map(|t| t.panes.values().map(|p| p.font))
			.collect();
		sizes.sort_unstable();
		sizes.dedup();
		let metrics: Vec<(i32, (i32, i32))> = sizes
			.into_iter()
			.map(|size| (size, self.canvas.metrics(size)))
			.collect();
		for tab in &mut self.tabs {
			tab.layout.resize(cols, rows);
			if let Some(saved) = &mut tab.saved {
				saved.resize(cols, rows);
			}
			for (id, rect) in tab.layout.rects(area, base_w, base_h) {
				if let Some(pane) = tab.panes.get_mut(&id) {
					let (cw, ch) = metrics
						.iter()
						.find(|(size, _)| *size == pane.font)
						.map(|(_, cells)| *cells)
						.unwrap_or((base_w, base_h));
					pane.cell = (cw.max(1), ch.max(1));
					resize_body(&mut pane.body, &rect, cw.max(1), ch.max(1));
				}
			}
		}
	}

	fn spawn_term(
		&mut self,
		command: &str,
		envs: &[(String, String)],
	) -> Option<(PaneId, TerminalPane)> {
		let id = self.next_pane;
		match TerminalPane::new(80, 24, command, envs) {
			Ok(mut term) => {
				term.start_reader(self.hwnd, id);
				self.next_pane += 1;
				Some((id, term))
			}
			Err(err) => {
				self.modal = Some(Modal::Notice(format!("启动失败：{err}\n{command}")));
				None
			}
		}
	}

	fn new_terminal_tab(&mut self) {
		let command = self.shell.clone();
		let envs = self.child_env.clone();
		let Some((id, term)) = self.spawn_term(&command, &envs) else {
			return;
		};
		self.add_tab(id, Body::Term(term), Owner::Normal, false, 0, None, true);
	}

	fn add_terminal(
		&mut self,
		tab: usize,
		command: &str,
		owner: Owner,
		ai_created: bool,
		group: u64,
		member: Option<String>,
		focus_new: bool,
	) -> Option<PaneId> {
		let envs = self.child_env.clone();
		let (id, term) = self.spawn_term(command, &envs)?;
		self.add_pane(
			tab,
			id,
			Body::Term(term),
			owner,
			ai_created,
			group,
			member,
			focus_new,
		);
		Some(id)
	}

	/// A new tmux window with its own tab. Only `c` and `!` make tabs; a
	/// window opened by a program goes into the tab that asked for it.
	fn add_tab(
		&mut self,
		id: PaneId,
		body: Body,
		owner: Owner,
		ai_created: bool,
		group: u64,
		member: Option<String>,
		focus_new: bool,
	) {
		let (cols, rows) = self.content_cells();
		let cell = (self.canvas.cell_w.max(1), self.canvas.cell_h.max(1));
		let pane = Pane {
			id,
			body,
			owner,
			seen: false,
			ai_created,
			creator: None,
			title: None,
			group_id: group,
			member,
			font: canvas::FONT_SIZE,
			cell,
		};
		let mut panes = HashMap::new();
		panes.insert(id, pane);
		self.tabs.push(Tab {
			layout: Layout::new(id, cols, rows),
			saved: None,
			panes,
			focus: id,
			hidden: Vec::new(),
		});
		if focus_new {
			self.active = self.tabs.len() - 1;
			// The window was created in order to be used; typing goes into it.
			self.mode = Mode::Insert;
			self.note_human(id);
		} else if ai_created {
			self.note_ai(id);
		}
		self.resize_panes();
	}

	/// Puts a program's new window into an existing tab as a pane and lays the
	/// drawn windows out with `layout_set_tiled`. A tab already drawing its
	/// limit hides its oldest window so the new one can be drawn; the hidden
	/// window keeps running. A window too small for one more tiled cell gets a
	/// new tab instead.
	fn add_pane(
		&mut self,
		tab_index: usize,
		id: PaneId,
		body: Body,
		owner: Owner,
		ai_created: bool,
		group: u64,
		member: Option<String>,
		focus_new: bool,
	) {
		let (cols, rows) = self.content_cells();
		let cell = (self.canvas.cell_w.max(1), self.canvas.cell_h.max(1));
		let pane = Pane {
			id,
			body,
			owner,
			seen: false,
			ai_created,
			creator: None,
			title: None,
			group_id: group,
			member,
			font: canvas::FONT_SIZE,
			cell,
		};
		let Some(tab) = self.tabs.get(tab_index) else {
			return;
		};
		let mut drawn = Self::drawn_ids(tab);
		let fits = Layout::tiled_fits(drawn.len() + 1, cols, rows);
		if !fits {
			let mut panes = HashMap::new();
			panes.insert(id, pane);
			self.tabs.push(Tab {
				layout: Layout::new(id, cols, rows),
				saved: None,
				panes,
				focus: id,
				hidden: Vec::new(),
			});
			if focus_new {
				self.active = self.tabs.len() - 1;
			}
			if focus_new {
				self.note_human(id);
			} else if ai_created {
				self.note_ai(id);
			}
			self.resize_panes();
			if focus_new {
				self.mode = Mode::Insert;
			}
			return;
		}
		if drawn.len() >= self.pane_limit {
			let focus = self.tabs[tab_index].focus;
			if let Some(victim) = lowest_priority(&drawn, &self.prio, Some(focus))
				.or_else(|| lowest_priority(&drawn, &self.prio, None))
			{
				drawn.retain(|pane| *pane != victim);
				let Some(tab) = self.tabs.get_mut(tab_index) else {
					return;
				};
				if !tab.hidden.contains(&victim) {
					tab.hidden.push(victim);
				}
				if tab.focus == victim {
					tab.focus = drawn.first().copied().unwrap_or(id);
				}
			}
		}
		drawn.push(id);
		self.retile_ids(tab_index, drawn);
		let Some(tab) = self.tabs.get_mut(tab_index) else {
			return;
		};
		tab.panes.insert(id, pane);
		if focus_new {
			tab.focus = id;
		}
		if focus_new {
			self.note_human(id);
		} else if ai_created {
			self.note_ai(id);
		}
		self.resize_panes();
		if focus_new {
			self.mode = Mode::Insert;
		}
	}

	fn import_path(&mut self, lparam: LPARAM) {
		unsafe {
			let data = lparam as *const COPYDATASTRUCT;
			if data.is_null() || (*data).dwData != COPYDATA_TAG || (*data).lpData.is_null() {
				return;
			}
			let bytes =
				std::slice::from_raw_parts((*data).lpData as *const u8, (*data).cbData as usize);
			if bytes.len() % 2 != 0 {
				return;
			}
			let units: Vec<u16> = bytes
				.chunks_exact(2)
				.map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
				.collect();
			let Ok(mut text) = String::from_utf16(&units) else {
				return;
			};
			while text.ends_with('\0') {
				text.pop();
			}
			if text.is_empty() {
				return;
			}
			self.open_editor(PathBuf::from(text));
		}
	}

	fn open_editor(&mut self, path: PathBuf) -> Option<PaneId> {
		if let Some(id) = self.find_solid(&path) {
			self.focus_pane_anywhere(id);
			return Some(id);
		}
		let editor = match Editor::open(path.clone(), false) {
			Ok(editor) => editor,
			Err(err) => {
				self.modal = Some(Modal::Notice(format!(
					"无法读取 {}：{err}。文件没有被改动。",
					path.display()
				)));
				return None;
			}
		};
		let id = self.next_pane;
		self.next_pane += 1;
		let tab = self.active;
		self.add_pane(
			tab,
			id,
			Body::Solid(editor),
			Owner::Normal,
			false,
			0,
			None,
			true,
		);
		Some(id)
	}

	fn find_solid(&self, path: &std::path::Path) -> Option<PaneId> {
		let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
		for tab in &self.tabs {
			for pane in tab.panes.values() {
				if let Body::Solid(editor) = &pane.body {
					let open =
						std::fs::canonicalize(&editor.path).unwrap_or_else(|_| editor.path.clone());
					if open == target {
						return Some(pane.id);
					}
				}
			}
		}
		None
	}

	fn focus_pane_anywhere(&mut self, id: PaneId) {
		for (index, tab) in self.tabs.iter().enumerate() {
			if tab.panes.contains_key(&id) {
				if self.active != index {
					self.active = index;
				}
				self.focus_pane(id);
				if self.focused_id() == Some(id) {
					self.note_human(id);
				}
				return;
			}
		}
	}

	fn open_group(&mut self) -> PaneId {
		let root = std::env::current_dir()
			.map(|p| p.to_string_lossy().into_owned())
			.unwrap_or_default();
		let group_id = self.next_group;
		self.next_group += 1;
		let name = format!("even{group_id}");
		let group = GroupPane::new(&name, &root, self.client.clone());
		let id = self.next_pane;
		self.next_pane += 1;
		let tab = self.active;
		self.add_pane(
			tab,
			id,
			Body::Even(Box::new(group)),
			Owner::Normal,
			false,
			group_id,
			None,
			true,
		);
		id
	}

	fn pane_title(p: &Pane) -> String {
		if let Some(title) = &p.title {
			return title.clone();
		}
		match &p.body {
			Body::Term(t) => {
				if !t.title.is_empty() {
					t.title.clone()
				} else if let Some(member) = &p.member {
					member.clone()
				} else {
					String::from("terminal")
				}
			}
			Body::Solid(e) => e.title(),
			Body::Even(g) => format!("even {}", g.name),
		}
	}

	fn pane_kind(p: &Pane) -> &'static str {
		match &p.body {
			Body::Term(_) => "terminal",
			Body::Solid(_) => "solid",
			Body::Even(_) => "even",
		}
	}

	fn focus_pane(&mut self, id: PaneId) {
		if let Some(index) = self.tab_index_of(id) {
			self.active = index;
			if let Some(tab) = self.tabs.get_mut(index) {
				// A hidden window has no rectangle; the keyboard cannot land on
				// something that is not drawn.
				if !tab.layout.leaves().contains(&id) {
					return;
				}
				tab.focus = id;
			}
			self.mark_seen();
			self.ime_text.clear();
			unsafe {
				self.ime_position(self.hwnd);
			}
		}
	}

	fn zoomed(&self) -> bool {
		self.tabs
			.get(self.active)
			.is_some_and(|tab| tab.saved.is_some())
	}

	fn unzoom_active(&mut self) {
		if self.zoomed() {
			self.toggle_zoom();
		}
	}

	fn toggle_zoom(&mut self) {
		let (cols, rows) = self.content_cells();
		let active = self.active;
		let Some(tab) = self.tabs.get_mut(active) else {
			return;
		};
		match tab.saved.take() {
			Some(mut saved) => {
				saved.resize(cols, rows);
				tab.layout = saved;
			}
			None => {
				// tmux window_zoom: one drawn pane has nothing to zoom over.
				if tab.layout.leaves().len() < 2 {
					return;
				}
				let focus = tab.focus;
				tab.saved = Some(std::mem::replace(
					&mut tab.layout,
					Layout::new(focus, cols, rows),
				));
			}
		}
		self.resize_panes();
	}

	/// The tiled layout: while zoomed, that is the saved one, since the live
	/// layout holds only the zoomed pane.
	fn tiled(&self) -> Option<&Layout> {
		self.tabs
			.get(self.active)
			.map(|tab| tab.saved.as_ref().unwrap_or(&tab.layout))
	}

	fn note_human(&mut self, id: PaneId) {
		self.clock += 1;
		self.prio.entry(id).or_default().human = self.clock;
	}

	fn note_ai(&mut self, id: PaneId) {
		self.clock += 1;
		self.prio.entry(id).or_default().ai = self.clock;
	}

	/// layout_set_tiled for an explicit set of drawn windows: rows of panes,
	/// the last row filling the remainder, exactly as `select-layout tiled`
	/// builds them. The drawn set is therefore always tiled, not the result of
	/// stacking splits on one pane.
	fn retile_ids(&mut self, tab_index: usize, mut drawn: Vec<PaneId>) {
		let (cols, rows) = self.content_cells();
		drawn.sort_unstable();
		drawn.dedup();
		let Some(tab) = self.tabs.get_mut(tab_index) else {
			return;
		};
		tab.saved = None;
		tab.layout = Layout::tiled(&drawn, cols, rows);
	}

	/// The windows a tab currently draws (the saved layout while zoomed).
	fn drawn_ids(tab: &Tab) -> Vec<PaneId> {
		tab.saved.as_ref().unwrap_or(&tab.layout).leaves()
	}

	/// Takes the lowest-priority drawn window out of the layout but leaves it
	/// running, so the tab can draw one more. Its switch time is kept: when it
	/// comes back it comes back at that place in the order.
	fn hide_window(&mut self, tab_index: usize, id: PaneId) {
		let Some(tab) = self.tabs.get(tab_index) else {
			return;
		};
		let mut drawn = Self::drawn_ids(tab);
		if !drawn.contains(&id) {
			return;
		}
		drawn.retain(|pane| *pane != id);
		let focus = tab.focus;
		if !tab.hidden.contains(&id) {
			if let Some(tab) = self.tabs.get_mut(tab_index) {
				tab.hidden.push(id);
			}
		}
		self.retile_ids(tab_index, drawn.clone());
		let Some(tab) = self.tabs.get_mut(tab_index) else {
			return;
		};
		if focus == id {
			tab.focus = drawn.first().copied().unwrap_or(id);
		}
	}

	/// `o`: one more odd terminal window in the current tab, laid out tiled.
	fn open_terminal(&mut self) {
		self.unzoom_active();
		let command = self.shell.clone();
		let envs = self.child_env.clone();
		let Some((id, term)) = self.spawn_term(&command, &envs) else {
			return;
		};
		let tab = self.active;
		self.add_pane(
			tab,
			id,
			Body::Term(term),
			Owner::Normal,
			false,
			0,
			None,
			true,
		);
	}

	/// `Ctrl+=` / `Ctrl+-`: the focused window's own font only. The tiling
	/// keeps the base grid; the window just draws fewer or more cells.
	fn zoom_font(&mut self, delta: i32) {
		let Some(id) = self.focused_id() else { return };
		self.zoom_font_at(id, delta);
	}

	/// `Ctrl+wheel` zooms the window under the pointer, the one the wheel is
	/// aimed at; it does not steal focus from the keyboard's window.
	fn zoom_font_at(&mut self, id: PaneId, delta: i32) {
		let Some(pane) = self.pane_mut(id) else {
			return;
		};
		pane.font = (pane.font + delta).clamp(canvas::MIN_FONT_SIZE, canvas::MAX_FONT_SIZE);
		self.resize_panes();
	}

	/// `s`: the Windows file picker, as Explorer shows it.
	fn pick_file(&mut self) -> Option<PathBuf> {
		unsafe {
			let filter: Vec<u16> = "所有文件\0*.*\0\0".encode_utf16().collect();
			let title: Vec<u16> = "打开文件"
				.encode_utf16()
				.chain(std::iter::once(0))
				.collect();
			let mut buf = [0u16; 32768];
			let mut ofn: OPENFILENAMEW = zeroed();
			ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
			ofn.hwndOwner = self.hwnd;
			ofn.lpstrFilter = filter.as_ptr();
			ofn.lpstrFile = buf.as_mut_ptr();
			ofn.nMaxFile = buf.len() as u32;
			ofn.lpstrTitle = title.as_ptr();
			ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
			if GetOpenFileNameW(&mut ofn) == 0 {
				return None;
			}
			let len = buf.iter().position(|unit| *unit == 0).unwrap_or(0);
			Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
		}
	}

	/// Puts a hidden window back into the layout; false when the window is too
	/// small for one more tiled cell.
	fn show_window(&mut self, tab_index: usize, id: PaneId) -> bool {
		let (cols, rows) = self.content_cells();
		let Some(tab) = self.tabs.get(tab_index) else {
			return false;
		};
		if !tab.panes.contains_key(&id) {
			return false;
		}
		let mut drawn = Self::drawn_ids(tab);
		if drawn.contains(&id) {
			return true;
		}
		if !Layout::tiled_fits(drawn.len() + 1, cols, rows) {
			return false;
		}
		let was_empty = drawn.is_empty();
		drawn.push(id);
		self.retile_ids(tab_index, drawn);
		let Some(tab) = self.tabs.get_mut(tab_index) else {
			return false;
		};
		tab.hidden.retain(|pane| *pane != id);
		if was_empty {
			tab.focus = id;
		}
		true
	}

	/// Only a close makes the drawn set shrink below the limit; the windows
	/// that were visible stay visible whatever happens to their times.
	fn fill_limit(&mut self, tab_index: usize) {
		loop {
			let next = {
				let Some(tab) = self.tabs.get(tab_index) else {
					return;
				};
				if Self::drawn_ids(tab).len() >= self.pane_limit || tab.hidden.is_empty() {
					return;
				}
				highest_priority(&tab.hidden, &self.prio)
			};
			let Some(id) = next else { return };
			if !self.show_window(tab_index, id) {
				return;
			}
		}
	}

	fn enforce_limit(&mut self, tab_index: usize) {
		loop {
			let victim = {
				let Some(tab) = self.tabs.get(tab_index) else {
					return;
				};
				let drawn = Self::drawn_ids(tab);
				if drawn.len() <= self.pane_limit {
					return;
				}
				lowest_priority(&drawn, &self.prio, Some(tab.focus))
					.or_else(|| lowest_priority(&drawn, &self.prio, None))
			};
			let Some(victim) = victim else { return };
			self.hide_window(tab_index, victim);
		}
	}

	fn set_pane_limit(&mut self, limit: usize) {
		self.pane_limit = limit.clamp(1, 6);
		for index in 0..self.tabs.len() {
			// A zoomed tab draws one window; the limit counts the tiled ones.
			if let Some(tab) = self.tabs.get_mut(index) {
				if let Some(saved) = tab.saved.take() {
					tab.layout = saved;
				}
			}
			self.enforce_limit(index);
			self.fill_limit(index);
		}
		self.resize_panes();
	}

	fn focus_dir(&mut self, dx: i32, dy: i32) {
		let Some(focus) = self.tabs.get(self.active).map(|tab| tab.focus) else {
			return;
		};
		let next = self
			.tiled()
			.and_then(|layout| layout.neighbour(focus, dx, dy));
		let Some(next) = next else { return };
		// window_set_active_pane unzooms when the target is not the zoomed pane.
		self.unzoom_active();
		self.focus_pane(next);
		self.note_human(next);
	}

	fn adjust_size(&mut self, axis: Dir, cells: i32) {
		// cmd-resize-pane unzooms before resizing; layout_resize_pane moves the
		// border by whole cells and may take them from the previous pane.
		self.unzoom_active();
		let lc_type = match axis {
			Dir::Right => LayoutType::LeftRight,
			Dir::Down => LayoutType::TopBottom,
		};
		let Some(tab) = self.tabs.get_mut(self.active) else {
			return;
		};
		tab.layout.resize_pane(tab.focus, lc_type, cells, true);
		self.resize_panes();
	}

	fn forward_focus(&mut self, focused: bool) {
		// Focus events belong to the terminal program, not to whoever holds the
		// window: an AI-held pane must see them exactly like a normal one.
		if let Some(pane) = self.focused_mut() {
			if let Body::Term(term) = &mut pane.body {
				if term.screen.modes.focus_events {
					term.write(if focused { b"\x1b[I" } else { b"\x1b[O" });
				}
			}
		}
	}

	fn key(&mut self, key: &Key, sys: bool) -> bool {
		if sys && key.alt && matches!(key.vk, 0x73) {
			// Alt+F4 goes to the system.
			return false;
		}
		if self.modal.is_some() {
			if key.down {
				self.modal_key(key);
			}
			return true;
		}
		if self.scroll_mode {
			if key.down {
				self.scroll_key(key);
			}
			return true;
		}
		// A key press returns the view to the live screen, as in any terminal; only scroll mode
		// navigates the scrollback.
		if key.down {
			if let Some(id) = self.focused_id() {
				if let Some(pane) = self.pane_mut(id) {
					if let Body::Term(term) = &mut pane.body {
						term.scroll = 0;
					}
				}
			}
		}
		if key.vk == VK_F1 {
			// F1 belongs to delta in every keyboard mode. It lands on the control
			// mode and only that: normal in a window no AI holds, agent in an AI
			// window. Typing modes are entered with i.
			if key.down {
				self.mode = Mode::Normal;
				self.sync_owner_to_mode();
			}
			return true;
		}
		// F2-F12 also stay with delta in a typing mode. Everything else is the
		// program's in insert/share, and everything at all is delta's in
		// normal/agent.
		let function_key = matches!(key.vk, 0x70..=0x7B);
		if self.mode == Mode::Normal {
			if key.down {
				self.normal_key(key);
			}
			return true;
		}
		if function_key {
			return true;
		}
		self.insert_key(key);
		true
	}

	fn normal_key(&mut self, key: &Key) -> bool {
		if key.ctrl && !key.alt {
			match key.vk {
				0x43 => {
					// The selection is delta's in this mode, so copying it is
					// delta's too. What Ctrl+C means to a program is settled in
					// the program, in insert.
					self.copy_selection();
					return true;
				}
				0x56 => {
					self.paste();
					return true;
				}
				0x25 => {
					self.adjust_size(Dir::Right, -2);
					return true;
				}
				0x27 => {
					self.adjust_size(Dir::Right, 2);
					return true;
				}
				0x26 => {
					self.adjust_size(Dir::Down, -1);
					return true;
				}
				0x28 => {
					self.adjust_size(Dir::Down, 1);
					return true;
				}
				0xBB | 0x6B => {
					self.zoom_font(2);
					return true;
				}
				0xBD | 0x6D => {
					self.zoom_font(-2);
					return true;
				}
				// Any other Ctrl chord is delta's too in normal; it just has no
				// action yet. Nothing is forwarded to the program.
				_ => return true,
			}
		}
		match key.vk {
			0x25 => {
				self.focus_dir(-1, 0);
				return true;
			}
			0x26 => {
				self.focus_dir(0, -1);
				return true;
			}
			0x27 => {
				self.focus_dir(1, 0);
				return true;
			}
			0x28 => {
				self.focus_dir(0, 1);
				return true;
			}
			_ => {}
		}
		let Some(ch) = key.ch else {
			return true;
		};
		match ch {
			'c' => self.new_terminal_tab(),
			'x' => {
				if let Some(id) = self.focused_id() {
					self.request_close_pane(id);
				}
			}
			'&' => self.request_close_tab(),
			'z' => self.toggle_zoom(),
			'n' => self.next_tab(1),
			'p' => self.modal = Some(Modal::PaneLimit { scroll: 0 }),
			'w' => self.modal = Some(Modal::Tabs),
			'[' => {
				self.scroll_mode = true;
				self.mode = Mode::Normal;
			}
			'!' => self.break_pane(),
			'q' => self.show_numbers = !self.show_numbers,
			'?' => self.modal = Some(Modal::Help),
			'e' => {
				self.open_group();
			}
			'o' => self.open_terminal(),
			's' => {
				if let Some(path) = self.pick_file() {
					self.open_editor(path);
				}
			}
			'i' => {
				// Agent is the AI window's normal, so i enters its typing mode,
				// share, the same way it enters insert elsewhere.
				self.mode = Mode::Insert;
				self.sync_owner_to_mode();
				self.ime_position(self.hwnd);
			}
			'1'..='9' => {
				let index = (ch as u8 - b'1') as usize;
				if index < self.tabs.len() {
					self.active = index;
					self.mark_seen();
				}
			}
			_ => {}
		}
		true
	}

	/// In insert/share, delta keeps only F1-F12. Every other key goes to the
	/// program in the window untouched, including Ctrl chords, mouse selection
	/// and the wheel. Whether a key means copy, interrupt or anything else is
	/// decided by that program, not here.
	fn insert_key(&mut self, key: &Key) -> bool {
		let Some(pane) = self.focused_mut() else {
			return true;
		};
		match &mut pane.body {
			Body::Term(term) => {
				term.key(key);
				// ConPTY's host never raises the console's Ctrl+C event, so the byte alone
				// disappears. Tell the shell that is listening on the control pipe, as the
				// console host would.
				if key.down && key.ctrl && !key.alt && !key.shift && key.vk == 0x43 {
					if let Some(pty) = &term.pty {
						pty.interrupt();
					}
				}
			}
			Body::Solid(editor) => {
				editor.key(key);
			}
			Body::Even(group) => {
				group.key(key);
			}
		}
		true
	}

	fn copy_selection(&mut self) {
		let Some(pane) = self.focused() else { return };
		let text = match &pane.body {
			Body::Term(term) => term.selection_text(),
			Body::Solid(editor) => {
				if editor.has_selection() {
					Some(editor.selected_text())
				} else {
					None
				}
			}
			Body::Even(group) => {
				let text = group.selected_text();
				(!text.is_empty()).then_some(text)
			}
		};
		if let Some(text) = text.filter(|text| !text.is_empty()) {
			clipboard::set(&text);
		}
	}

	fn paste(&mut self) -> bool {
		let Some(pane) = self.focused_mut() else {
			return false;
		};
		match &mut pane.body {
			Body::Term(term) => {
				// 终端只收文本字节；图片对 cmd 这类程序没有字节语义。
				let Some(clipboard::Paste::Text(text)) = clipboard::paste() else {
					return false;
				};
				term.paste(&text);
			}
			// solid 只编辑文本；图片在 even 里变成 Markdown 图片路径。
			Body::Solid(editor) => editor.paste_clipboard(),
			Body::Even(group) => group.paste(),
		}
		true
	}

	fn scroll_key(&mut self, key: &Key) {
		if matches!(key.vk, VK_ESCAPE | VK_F1 | 0x51) {
			self.leave_scroll();
			return;
		}
		let Some(pane) = self.focused_mut() else {
			self.scroll_mode = false;
			return;
		};
		match key.vk {
			0x26 => scroll_body(&mut pane.body, 1),
			0x28 => scroll_body(&mut pane.body, -1),
			0x21 => scroll_body(&mut pane.body, 10),
			0x22 => scroll_body(&mut pane.body, -10),
			0x24 => scroll_body(&mut pane.body, 100000),
			0x23 => scroll_body(&mut pane.body, -100000),
			_ => {}
		}
	}

	fn break_pane(&mut self) {
		// cmd-break-pane runs server_unzoom_window first.
		self.unzoom_active();
		let (cols, rows) = self.content_cells();
		let active = self.active;
		let Some(tab) = self.tabs.get_mut(active) else {
			return;
		};
		if tab.panes.len() < 2 {
			return;
		}
		let id = tab.focus;
		let mut drawn = Self::drawn_ids(tab);
		drawn.retain(|pane| *pane != id);
		tab.hidden.retain(|pane| *pane != id);
		let Some(pane) = tab.panes.remove(&id) else {
			return;
		};
		tab.focus = drawn.first().copied().unwrap_or(0);
		self.retile_ids(active, drawn);
		let mut panes = HashMap::new();
		panes.insert(id, pane);
		self.tabs.push(Tab {
			layout: Layout::new(id, cols, rows),
			saved: None,
			panes,
			focus: id,
			hidden: Vec::new(),
		});
		self.active = self.tabs.len() - 1;
		self.fill_limit(active);
		self.resize_panes();
		self.mode = Mode::Insert;
	}

	fn next_tab(&mut self, delta: i32) {
		if self.tabs.is_empty() {
			return;
		}
		let len = self.tabs.len() as i32;
		let next = ((self.active as i32 + delta) % len + len) % len;
		self.active = next as usize;
		let focus = self.tabs[next as usize].focus;
		self.note_human(focus);
		self.mark_seen();
	}

	fn insert_text(&mut self, text: &str) {
		if text.is_empty() {
			return;
		}
		let Some(pane) = self.focused_mut() else {
			return;
		};
		match &mut pane.body {
			Body::Term(term) => term.write(text.as_bytes()),
			Body::Solid(editor) => editor.paste(text),
			Body::Even(group) => group.insert_text(text),
		}
	}

	fn ime_string(himc: *mut std::ffi::c_void, flag: u32) -> String {
		unsafe {
			let len = ImmGetCompositionStringW(himc, flag, null_mut(), 0);
			if len <= 0 {
				return String::new();
			}
			let mut buf = vec![0u16; (len as usize + 1) / 2];
			let copied =
				ImmGetCompositionStringW(himc, flag, buf.as_mut_ptr() as *mut _, len as u32);
			if copied <= 0 {
				return String::new();
			}
			String::from_utf16_lossy(&buf[..(copied as usize) / 2])
		}
	}

	fn ime_composition(&mut self, hwnd: HWND, lparam: LPARAM) {
		unsafe {
			let himc = ImmGetContext(hwnd);
			if himc.is_null() {
				return;
			}
			if lparam & (GCS_RESULTSTR as isize) != 0 {
				let text = Self::ime_string(himc, GCS_RESULTSTR);
				if !text.is_empty() {
					self.insert_text(&text);
				}
				self.ime_text.clear();
			}
			if lparam & (GCS_COMPSTR as isize) != 0 {
				self.ime_text = Self::ime_string(himc, GCS_COMPSTR);
			}
			ImmReleaseContext(hwnd, himc);
		}
		self.ime_position(hwnd);
	}

	fn ime_position(&mut self, hwnd: HWND) {
		if self.mode == Mode::Normal {
			return;
		}
		let Some((x, y)) = self.caret_pixel() else {
			return;
		};
		self.ime_caret = Some((x, y));
		unsafe {
			let himc = ImmGetContext(hwnd);
			if himc.is_null() {
				return;
			}
			let (cw, ch) = self
				.focused()
				.map(|pane| pane.cell)
				.unwrap_or((self.canvas.cell_w, self.canvas.cell_h));
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
					right: x + cw,
					bottom: y + ch,
				},
			};
			let _ = ImmSetCandidateWindow(himc, &candidate);
			ImmReleaseContext(hwnd, himc);
		}
	}

	fn caret_pixel(&self) -> Option<(i32, i32)> {
		let id = self.focused_id()?;
		let pane = self.pane(id)?;
		let rect = self.pane_rect(id)?;
		let (cw, ch) = (pane.cell.0.max(1), pane.cell.1.max(1));
		match &pane.body {
			Body::Term(term) => {
				let (col, row) = term.screen.cursor();
				let x = rect.left + col as i32 * cw;
				let y = rect.top + row as i32 * ch;
				Some((x.min(rect.right - cw), y.min(rect.bottom - ch)))
			}
			Body::Solid(editor) => {
				let row = editor.cursor.0.saturating_sub(editor.scroll);
				let mut visual = 0usize;
				for c in editor.lines[editor.cursor.0].iter().take(editor.cursor.1) {
					visual += if *c == '\t' {
						4 - visual % 4
					} else {
						crate::solid::char_cells(*c)
					};
				}
				let col = visual.saturating_sub(editor.hscroll);
				Some((rect.left + col as i32 * cw, rect.top + row as i32 * ch))
			}
			Body::Even(group) => {
				let cols = (rect.width() / cw).max(1) as usize;
				let rows = (rect.height() / ch).max(1) as usize;
				let (col, row) = group.caret_cell(cols, rows);
				Some((rect.left + col as i32 * cw, rect.top + row as i32 * ch))
			}
		}
	}

	fn mouse_target(&self, x: i32, y: i32) -> Option<PaneId> {
		if y >= self.content_area().bottom {
			return None;
		}
		let tab = self.tabs.get(self.active)?;
		let area = self.content_area();
		self.rects_of(tab, area)
			.into_iter()
			.find(|(_, r)| r.contains(x, y))
			.map(|(id, _)| id)
	}

	fn cell_of(&self, id: PaneId, x: i32, y: i32) -> Option<(usize, usize, bool)> {
		let rect = self.pane_rect(id)?;
		let pane = self.pane(id)?;
		let Body::Term(term) = &pane.body else {
			return None;
		};
		let cw = pane.cell.0.max(1);
		let ch = pane.cell.1.max(1);
		let col = ((x - rect.left) / cw).max(0) as usize;
		let row = ((y - rect.top) / ch).max(0) as usize;
		Some((term.view_top() + row, col, term.alt))
	}

	fn mouse_down(&mut self, x: i32, y: i32) {
		let Some(id) = self.mouse_target(x, y) else {
			return;
		};
		let already_focused = self.focused_id() == Some(id);
		self.focus_pane(id);
		if !already_focused {
			self.note_human(id);
		}
		// A click is the same click in every window: it lands on the control
		// mode. Which control mode that is — normal or agent — is the owner's
		// business, not the mouse's.
		if !self.scroll_mode {
			self.mode = Mode::Normal;
			self.sync_owner_to_mode();
		}
		if let Some(pane) = self.pane_mut(id) {
			pane.ai_created = false;
		}
		let shift = unsafe { GetKeyState(VK_SHIFT as i32) < 0 };
		self.drag = Some(Drag {
			pane: id,
			moved: false,
		});
		unsafe {
			SetCapture(self.hwnd);
		}
		self.ime_position(self.hwnd);
		let Some((row, col)) = self.screen_cell(id, x, y) else {
			return;
		};
		let insert = self.mode == Mode::Insert;
		enum Hit {
			Program,
			Term,
			Solid,
			Even,
		}
		let hit = match self.pane(id).map(|p| &p.body) {
			Some(Body::Term(term)) => {
				if insert {
					if term.screen.modes.mouse_mode == MouseMode::None {
						return;
					}
					Hit::Program
				} else {
					Hit::Term
				}
			}
			Some(Body::Solid(_)) => Hit::Solid,
			Some(Body::Even(_)) => Hit::Even,
			None => return,
		};
		match hit {
			Hit::Program => {
				// Insert is the program's: a click is a click only if it asked
				// for mouse reports, and it never falls back to delta's selection.
				self.send_mouse(id, x, y, 0, false, false);
			}
			Hit::Term => {
				if let Some((abs, abs_col, alt)) = self.cell_of(id, x, y) {
					if let Some(pane) = self.pane_mut(id) {
						if let Body::Term(term) = &mut pane.body {
							term.selection = Some(crate::term::Selection {
								anchor: crate::term::Point {
									row: abs,
									col: abs_col,
								},
								cursor: crate::term::Point {
									row: abs,
									col: abs_col,
								},
								alt,
							});
						}
					}
				}
			}
			Hit::Solid => {
				if let Some(pane) = self.pane_mut(id) {
					if let Body::Solid(editor) = &mut pane.body {
						editor.click_screen(row, col, shift);
					}
				}
			}
			Hit::Even => {
				let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) < 0 };
				if let Some((cols, rows)) = self.body_cells(id) {
					if let Some(pane) = self.pane_mut(id) {
						if let Body::Even(group) = &mut pane.body {
							// Ctrl+click is even's alone: it hands an image path or
							// link to the system program instead of selecting.
							if ctrl {
								group.open_at(cols, rows, row, col);
							} else {
								group.press(cols, rows, row, col);
							}
						}
					}
				}
			}
		}
	}

	fn mouse_move(&mut self, x: i32, y: i32) {
		let Some(id) = self.drag.as_ref().map(|d| d.pane) else {
			return;
		};
		if let Some(drag) = &mut self.drag {
			drag.moved = true;
		}
		let insert = self.mode == Mode::Insert;
		enum Hit {
			Program,
			Term,
			Solid,
			Even,
		}
		let hit = match self.pane(id).map(|p| &p.body) {
			Some(Body::Term(term)) => {
				if insert {
					if term.screen.modes.mouse_mode == MouseMode::None {
						return;
					}
					Hit::Program
				} else {
					Hit::Term
				}
			}
			Some(Body::Solid(_)) => Hit::Solid,
			Some(Body::Even(_)) => Hit::Even,
			None => return,
		};
		match hit {
			Hit::Program => {
				self.send_mouse(id, x, y, 0, false, true);
			}
			Hit::Term => {
				if let Some((abs, col, alt)) = self.cell_of(id, x, y) {
					if let Some(pane) = self.pane_mut(id) {
						if let Body::Term(term) = &mut pane.body {
							if let Some(sel) = &mut term.selection {
								sel.cursor = crate::term::Point { row: abs, col };
								sel.alt = alt;
							}
						}
					}
				}
			}
			Hit::Solid => {
				if let Some((row, col)) = self.screen_cell(id, x, y) {
					if let Some(pane) = self.pane_mut(id) {
						if let Body::Solid(editor) = &mut pane.body {
							editor.click_screen(row, col, true);
						}
					}
				}
			}
			Hit::Even => {
				if let Some((row, col)) = self.screen_cell(id, x, y) {
					if let Some((cols, rows)) = self.body_cells(id) {
						if let Some(pane) = self.pane_mut(id) {
							if let Body::Even(group) = &mut pane.body {
								group.drag(cols, rows, row, col);
							}
						}
					}
				}
			}
		}
	}

	fn mouse_up(&mut self, x: i32, y: i32) {
		if let Some(drag) = self.drag.take() {
			let insert = self.mode == Mode::Insert;
			let reporting = self
				.pane(drag.pane)
				.map(|p| match &p.body {
					Body::Term(term) => term.screen.modes.mouse_mode != MouseMode::None,
					_ => false,
				})
				.unwrap_or(false);
			if reporting && insert {
				self.send_mouse(drag.pane, x, y, 0, true, false);
			} else if !drag.moved {
				// A plain click without a drag leaves no one-cell selection behind.
				if let Some(pane) = self.pane_mut(drag.pane) {
					if let Body::Term(term) = &mut pane.body {
						term.selection = None;
					}
				}
			}
		}
		unsafe {
			ReleaseCapture();
		}
	}

	fn mouse_double(&mut self, x: i32, y: i32) {
		let Some(id) = self.mouse_target(x, y) else {
			return;
		};
		let Some((row, col)) = self.screen_cell(id, x, y) else {
			return;
		};
		if let Some(pane) = self.pane_mut(id) {
			if let Body::Solid(editor) = &mut pane.body {
				editor.double_click_screen(row, col);
			}
		}
	}

	fn screen_cell(&self, id: PaneId, x: i32, y: i32) -> Option<(usize, usize)> {
		let rect = self.pane_rect(id)?;
		let pane = self.pane(id)?;
		if x < rect.left || y < rect.top || x >= rect.right || y >= rect.bottom {
			return None;
		}
		let cw = pane.cell.0.max(1);
		let ch = pane.cell.1.max(1);
		Some((
			((y - rect.top) / ch).max(0) as usize,
			((x - rect.left) / cw).max(0) as usize,
		))
	}

	fn body_cells(&self, id: PaneId) -> Option<(usize, usize)> {
		let rect = self.pane_rect(id)?;
		let pane = self.pane(id)?;
		let cw = pane.cell.0.max(1);
		let ch = pane.cell.1.max(1);
		Some((
			(rect.width() / cw).max(1) as usize,
			(rect.height() / ch).max(1) as usize,
		))
	}

	fn mouse_wheel(&mut self, x: i32, y: i32, delta: i32) {
		if delta == 0 {
			return;
		}
		if matches!(self.modal, Some(Modal::PaneLimit { .. })) {
			let max = self.pane_list_len().saturating_sub(PANE_LIST_ROWS) as isize;
			if let Some(Modal::PaneLimit { scroll }) = &mut self.modal {
				*scroll = (*scroll - delta as isize * 3).clamp(0, max);
			}
			return;
		}
		let id = self.mouse_target(x, y).or_else(|| self.focused_id());
		let Some(id) = id else { return };
		let insert = self.mode == Mode::Insert;
		let notches = delta.abs().max(1) as isize;
		let up = delta > 0;
		let steps = notches * 3;
		let mut wheel_button = None;
		if let Some(pane) = self.pane_mut(id) {
			match &mut pane.body {
				Body::Term(term) => {
					if insert && term.screen.modes.mouse_mode != MouseMode::None {
						// The program asked for mouse reports, so the wheel is its.
						wheel_button = Some(if up { 64 } else { 65 });
					} else if term.alt && term.screen.modes.alt_scroll {
						let seq = if up { b"\x1b[A" as &[u8] } else { b"\x1b[B" };
						for _ in 0..steps {
							term.write(seq);
						}
					} else if up {
						term.scroll_up(steps as usize);
					} else {
						term.scroll_down(steps as usize);
					}
				}
				Body::Solid(editor) => {
					editor.wheel(if up { -(steps as i32) } else { steps as i32 });
				}
				Body::Even(group) => {
					group.wheel(delta);
				}
			}
		}
		if let Some(button) = wheel_button {
			for _ in 0..steps {
				self.send_mouse(id, x, y, button, false, false);
			}
		}
	}

	fn pane_list_len(&self) -> usize {
		let Some(tab) = self.tabs.get(self.active) else {
			return 0;
		};
		let drawn = Self::drawn_ids(tab);
		drawn.len() + tab.hidden.iter().filter(|id| !drawn.contains(id)).count()
	}

	fn send_mouse(
		&self,
		id: PaneId,
		x: i32,
		y: i32,
		button: i32,
		release: bool,
		motion: bool,
	) -> bool {
		let Some(rect) = self.pane_rect(id) else {
			return false;
		};
		let Some(pane) = self.pane(id) else {
			return false;
		};
		let Body::Term(term) = &pane.body else {
			return false;
		};
		let mode = term.screen.modes.mouse_mode;
		if mode == MouseMode::None {
			return false;
		}
		let cw = pane.cell.0.max(1);
		let ch = pane.cell.1.max(1);
		let cx = (((x - rect.left) / cw) + 1).clamp(1, 1000) as i32;
		let cy = (((y - rect.top) / ch) + 1).clamp(1, 1000) as i32;
		let mut b = button;
		if motion {
			b += 32;
		}
		let seq = match term.screen.modes.mouse_encoding {
			MouseEncoding::Sgr => {
				format!(
					"\x1b[<{};{};{}{}",
					b,
					cx,
					cy,
					if release { 'm' } else { 'M' }
				)
			}
			MouseEncoding::Utf8 => {
				let mut out = String::from("\x1b[M");
				for value in [32 + b, 32 + cx, 32 + cy] {
					if let Some(ch) = char::from_u32(value as u32) {
						out.push(ch);
					}
				}
				out
			}
			_ => {
				let bytes = [
					27u8,
					b'[',
					b'M',
					(32 + b).min(255) as u8,
					(32 + cx).min(255) as u8,
					(32 + cy).min(255) as u8,
				];
				String::from_utf8_lossy(&bytes).into_owned()
			}
		};
		term.write(seq.as_bytes());
		true
	}

	fn request_close_app(&mut self) {
		if let Some(event) = self.events.first() {
			self.modal = Some(Modal::Events { pane: event.pane });
			return;
		}
		for pane in self.tabs.iter().flat_map(|tab| tab.panes.values()) {
			if let Owner::Agent(agent) | Owner::Share(agent) = &pane.owner {
				self.modal = Some(Modal::CloseAgent {
					pane: pane.id,
					agent: agent.clone(),
				});
				return;
			}
			if let Body::Solid(editor) = &pane.body {
				if editor.modified {
					let id = pane.id;
					self.focus_pane_anywhere(id);
					self.modal = Some(Modal::Notice(String::from(
						"文件尚未保存，窗口没有关闭。先在 solid 按 Ctrl+S 保存。",
					)));
					return;
				}
			}
		}
		unsafe {
			DestroyWindow(self.hwnd);
		}
	}

	fn request_close_pane(&mut self, id: PaneId) {
		if self
			.pane(id)
			.is_some_and(|pane| matches!(&pane.body, Body::Solid(editor) if editor.modified))
		{
			self.modal = Some(Modal::Notice(String::from(
				"文件尚未保存，窗口没有关闭。先在 solid 按 Ctrl+S 保存。",
			)));
			return;
		}
		if self.events.iter().any(|e| e.pane == id) {
			self.modal = Some(Modal::Events { pane: id });
			return;
		}
		let owner = self.pane(id).map(|p| p.owner.clone());
		match owner {
			Some(Owner::Agent(agent)) | Some(Owner::Share(agent)) => {
				self.modal = Some(Modal::CloseAgent { pane: id, agent });
			}
			Some(Owner::Normal) => self.remove_pane_direct(id),
			None => {}
		}
	}

	fn request_close_tab(&mut self) {
		let Some(tab) = self.tabs.get(self.active) else {
			return;
		};
		if let Some(id) = tab.panes.values().find_map(|pane| {
			matches!(&pane.body, Body::Solid(editor) if editor.modified).then_some(pane.id)
		}) {
			self.focus_pane_anywhere(id);
			self.modal = Some(Modal::Notice(String::from(
				"标签中有文件尚未保存，没有关闭。先在 solid 按 Ctrl+S 保存。",
			)));
			return;
		}
		if let Some(event) = self.events.iter().find(|e| tab.panes.contains_key(&e.pane)) {
			let pane = event.pane;
			self.modal = Some(Modal::Events { pane });
			return;
		}
		for pane in tab.panes.values() {
			match &pane.owner {
				Owner::Agent(agent) | Owner::Share(agent) => {
					let agent = agent.clone();
					let pane = pane.id;
					self.modal = Some(Modal::CloseAgent { pane, agent });
					return;
				}
				Owner::Normal => {}
			}
		}
		self.close_tab_direct(self.active);
	}

	fn remove_pane_direct(&mut self, id: PaneId) {
		let is_group = self
			.pane(id)
			.map(|p| matches!(p.body, Body::Even(_)) && p.group_id != 0)
			.unwrap_or(false);
		if is_group {
			let group = self.pane(id).map(|p| p.group_id).unwrap_or(0);
			let pages: Vec<PaneId> = self
				.tabs
				.iter()
				.flat_map(|t| t.panes.values())
				.filter(|p| p.group_id == group && p.id != id)
				.map(|p| p.id)
				.collect();
			for page in pages {
				self.stop_events_for(page);
				self.remove_single(page);
			}
		}
		self.stop_events_for(id);
		self.remove_single(id);
		if self.tabs.is_empty() {
			self.new_terminal_tab();
		}
		self.resize_panes();
	}

	fn remove_single(&mut self, id: PaneId) {
		let Some(index) = self.tab_index_of(id) else {
			return;
		};
		let last = self.tabs[index].panes.len() <= 1;
		if last {
			let gone: Vec<PaneId> = self.tabs[index].panes.keys().copied().collect();
			self.tabs.remove(index);
			for gone in gone {
				self.prio.remove(&gone);
			}
			if index < self.active {
				self.active -= 1;
			}
			if self.active >= self.tabs.len() {
				self.active = self.tabs.len().saturating_sub(1);
			}
			return;
		}
		let mut drawn = Self::drawn_ids(&self.tabs[index]);
		drawn.retain(|pane| *pane != id);
		let tab = &mut self.tabs[index];
		tab.hidden.retain(|pane| *pane != id);
		tab.panes.remove(&id);
		if tab.focus == id {
			tab.focus = drawn.first().copied().unwrap_or(0);
		}
		self.retile_ids(index, drawn);
		self.prio.remove(&id);
		// The closed window frees one draw slot; the next one in the priority
		// order takes it.
		self.fill_limit(index);
	}

	fn close_tab_direct(&mut self, index: usize) {
		let Some(tab) = self.tabs.get(index) else {
			return;
		};
		let ids: Vec<PaneId> = tab.panes.keys().copied().collect();
		let Some(anchor) = ids.first().copied() else {
			return;
		};
		let mut groups: Vec<u64> = tab
			.panes
			.values()
			.filter(|p| matches!(p.body, Body::Even(_)) && p.group_id != 0)
			.map(|p| p.group_id)
			.collect();
		groups.sort_unstable();
		groups.dedup();
		for id in &ids {
			self.stop_events_for(*id);
		}
		if !groups.is_empty() {
			let pages: Vec<PaneId> = self
				.tabs
				.iter()
				.enumerate()
				.filter(|(i, _)| *i != index)
				.flat_map(|(_, t)| t.panes.values())
				.filter(|p| groups.contains(&p.group_id))
				.map(|p| p.id)
				.collect();
			for page in pages {
				self.stop_events_for(page);
				self.remove_single(page);
			}
		}
		// Page removal may have dropped tabs before this one; relocate it.
		let index = self.tab_index_of(anchor).unwrap_or(index);
		if index < self.tabs.len() {
			let gone: Vec<PaneId> = self.tabs[index].panes.keys().copied().collect();
			self.tabs.remove(index);
			for gone in gone {
				self.prio.remove(&gone);
			}
			if index < self.active {
				self.active -= 1;
			}
		}
		if self.active >= self.tabs.len() {
			self.active = self.tabs.len().saturating_sub(1);
		}
		if self.tabs.is_empty() {
			self.new_terminal_tab();
		}
		self.resize_panes();
	}

	fn stop_events_for(&mut self, pane: PaneId) {
		let ids: Vec<u64> = self
			.events
			.iter()
			.filter(|e| e.pane == pane)
			.map(|e| e.id)
			.collect();
		for id in ids {
			self.stop_event(id);
		}
		self.clear_owner_if_idle(pane);
	}

	fn clear_owner_if_idle(&mut self, pane: PaneId) {
		let busy = self.events.iter().any(|e| e.pane == pane);
		if busy {
			return;
		}
		if let Some(p) = self.pane_mut(pane) {
			if matches!(p.owner, Owner::Agent(_) | Owner::Share(_)) {
				p.owner = Owner::Normal;
				p.seen = false;
			}
		}
	}

	fn stop_event(&mut self, id: u64) {
		let Some(event) = self.events.iter().find(|e| e.id == id).cloned() else {
			return;
		};
		if let Some(pane) = self.pane(event.pane) {
			match &pane.body {
				Body::Even(group) => {
					group.stop(event.member.as_deref());
					return;
				}
				Body::Term(term) if !term.process_exited() => {
					term.write(b"\x03");
					if let Some(pty) = &term.pty {
						pty.interrupt();
					}
					if let Some(event) = self.events.iter_mut().find(|e| e.id == id) {
						event.label = String::from("已请求中断，等待终端命令结束");
					}
					return;
				}
				_ => {}
			}
		}
		self.events.retain(|e| e.id != id);
		self.clear_owner_if_idle(event.pane);
	}

	fn handle_even_events(&mut self) {
		let mut events = Vec::new();
		for tab in &self.tabs {
			for pane in tab.panes.values() {
				if let Body::Even(group) = &pane.body {
					for event in group.take_events() {
						events.push((pane.id, pane.group_id, event));
					}
				}
			}
		}
		for (pane, group, event) in events {
			match event {
				EvenEvent::OpenPage { name, session } => {
					self.open_member_page(pane, group, &name, &session);
				}
				EvenEvent::Stopping { name } => {
					if let Some(Pane {
						body: Body::Even(group),
						..
					}) = self.pane(pane)
					{
						let agent = format!("{}:{name}", group.name);
						self.cancel_takeovers(&agent);
					}
				}
				EvenEvent::Stopped { name } => {
					if let Some(Pane {
						body: Body::Even(group),
						..
					}) = self.pane(pane)
					{
						let agent = format!("{}:{name}", group.name);
						self.release_agent(&agent);
					}
				}
				EvenEvent::ClosePage { name } => {
					self.close_member_page(group, &name);
				}
			}
		}
	}

	fn cancel_takeovers(&mut self, agent: &str) {
		let requests = std::mem::take(&mut self.pending_takeovers);
		for request in requests {
			if request.agent == agent {
				request
					.cancelled
					.store(true, std::sync::atomic::Ordering::Release);
				let _ = request
					.reply
					.send(Err(String::from("成员已请求停止，未执行接管操作")));
			} else {
				self.pending_takeovers.push(request);
			}
		}
	}

	fn release_agent(&mut self, agent: &str) {
		self.cancel_takeovers(agent);
		self.auth.retain(|(_, owner), _| owner != agent);
		let events: Vec<u64> = self
			.events
			.iter()
			.filter(|event| event.member.as_deref() == Some(agent))
			.map(|event| event.id)
			.collect();
		for id in events {
			self.stop_event(id);
		}
		let panes: Vec<PaneId> = self
			.tabs
			.iter()
			.flat_map(|tab| tab.panes.values())
			.filter(|pane| matches!(&pane.owner, Owner::Agent(owner) if owner == agent))
			.map(|pane| pane.id)
			.collect();
		for id in panes {
			self.clear_owner_if_idle(id);
		}
	}

	fn open_member_page(&mut self, group_pane: PaneId, group: u64, name: &str, session: &str) {
		let existing = self
			.tabs
			.iter()
			.flat_map(|t| t.panes.values())
			.find(|p| p.group_id == group && p.member.as_deref() == Some(name))
			.map(|p| p.id);
		if let Some(id) = existing {
			self.focus_pane(id);
			self.note_ai(id);
			return;
		}
		let Some(server_url) = self.server.as_ref().map(|server| server.client.url.clone()) else {
			self.modal = Some(Modal::Notice(String::from(
				"opencode server 没有起来，打不开 AI 页面。",
			)));
			return;
		};
		// The ConPTY console starts on the system ANSI code page; opencode writes
		// UTF-8 bytes, so its page would arrive as mojibake. chcp runs in the
		// same console before opencode, whose own console test fixture sets both
		// code pages to 65001 the same way. The outer quotes let cmd keep the
		// quoted program path; taskkill /T still takes the whole tree on close.
		let program = format!(
			"\"{}\" --server {} --session {}",
			crate::opencode::program(),
			server_url,
			session
		);
		let command = format!("cmd /d /s /c \"chcp 65001>nul & {program}\"");
		let Some(tab_index) = self.tab_index_of(group_pane) else {
			return;
		};
		let _ = self.add_terminal(
			tab_index,
			&command,
			Owner::Normal,
			false,
			group,
			Some(String::from(name)),
			true,
		);
	}

	fn close_member_page(&mut self, group: u64, name: &str) {
		let pages: Vec<PaneId> = self
			.tabs
			.iter()
			.flat_map(|t| t.panes.values())
			.filter(|p| p.group_id == group && p.member.as_deref() == Some(name))
			.map(|p| p.id)
			.collect();
		for page in pages {
			self.stop_events_for(page);
			self.remove_single(page);
		}
		if self.tabs.is_empty() {
			self.new_terminal_tab();
		}
		self.resize_panes();
	}

	fn sync_events(&mut self) {
		let mut busy: Vec<(PaneId, Vec<String>)> = Vec::new();
		for tab in &self.tabs {
			for pane in tab.panes.values() {
				if let Body::Even(group) = &pane.body {
					busy.push((pane.id, group.busy_members()));
				}
			}
		}
		for (pane, members) in &busy {
			if members.is_empty() {
				continue;
			}
			let label = format!(
				"even 里 {} 正在处理",
				members
					.iter()
					.map(|m| format!("@{m}"))
					.collect::<Vec<_>>()
					.join(" ")
			);
			let single = if members.len() == 1 {
				Some(members[0].clone())
			} else {
				None
			};
			let found = self
				.events
				.iter()
				.position(|e| e.pane == *pane && e.kind == EventKind::AiTask && e.member == single);
			match found {
				Some(index) => self.events[index].label = label,
				None => {
					self.events.retain(|e| e.pane != *pane);
					let id = self.next_event;
					self.next_event += 1;
					self.events.push(EventRecord {
						id,
						pane: *pane,
						kind: EventKind::AiTask,
						label,
						member: single,
						expires: None,
					});
				}
			}
		}
		let stale: Vec<(u64, PaneId)> = self
			.events
			.iter()
			.filter(|e| {
				let is_group = self
					.pane(e.pane)
					.map(|p| matches!(p.body, Body::Even(_)))
					.unwrap_or(false);
				is_group
					&& !busy
						.iter()
						.any(|(pane, members)| *pane == e.pane && !members.is_empty())
			})
			.map(|e| (e.id, e.pane))
			.collect();
		for (id, pane) in stale {
			self.events.retain(|e| e.id != id);
			self.clear_owner_if_idle(pane);
		}
		let now = Instant::now();
		let expired: Vec<PaneId> = self
			.events
			.iter()
			.filter(|e| e.expires.map(|t| now >= t).unwrap_or(false))
			.map(|e| e.pane)
			.collect();
		if !expired.is_empty() {
			self.events
				.retain(|e| e.expires.map(|t| now < t).unwrap_or(true));
			for pane in expired {
				self.clear_owner_if_idle(pane);
			}
		}
	}

	fn handle_mcp(&mut self) {
		let requests = match &self.mcp {
			Some(server) => {
				let mut list = Vec::new();
				while let Ok(request) = server.rx.try_recv() {
					list.push(request);
				}
				list
			}
			None => Vec::new(),
		};
		for mut request in requests {
			if let Some(session) = &request.session {
				let Some(agent) = self.session_agent(session) else {
					let _ = request
						.reply
						.send(Err(String::from("这个 even 成员正在停止或已经不存在")));
					continue;
				};
				request.agent = agent;
			}
			if !request.is_active() {
				continue;
			}
			if matches!(request.tool.as_str(), "write_window" | "run_in_terminal") {
				if let Ok(id) = self.parse_id(&request.args) {
					if self.needs_takeover(id, &request.agent) {
						self.pending_takeovers.push(request);
						continue;
					}
				}
			}
			let result = self.mcp_tool(&request.tool, &request.args, &request.agent);
			let _ = request.reply.send(result);
		}
		self.pending_takeovers.retain(|request| request.is_active());
		if let Some(Modal::Takeover { pane, agent }) = &self.modal {
			if !self
				.pending_takeovers
				.iter()
				.any(|r| r.args["id"].as_u64() == Some(*pane as u64) && r.agent == *agent)
			{
				self.modal = None;
			}
		}
		if self.modal.is_none() {
			if let Some(request) = self.pending_takeovers.first() {
				if let Ok(pane) = self.parse_id(&request.args) {
					self.modal = Some(Modal::Takeover {
						pane,
						agent: request.agent.clone(),
					});
				}
			}
		}
	}

	fn session_agent(&self, session: &str) -> Option<String> {
		self.tabs
			.iter()
			.flat_map(|tab| tab.panes.values())
			.find_map(|pane| {
				let Body::Even(group) = &pane.body else {
					return None;
				};
				if !group.session_can_act(session) {
					return None;
				}
				group
					.member_sessions()
					.into_iter()
					.find(|(_, id)| id == session)
					.map(|(name, _)| format!("{}:{name}", group.name))
			})
	}

	fn needs_takeover(&self, id: PaneId, agent: &str) -> bool {
		self.pane(id).is_some_and(|pane| {
			pane.owner == Owner::Normal
				&& !self.auth.contains_key(&(id, agent.to_string()))
				&& (pane.seen
					|| pane
						.creator
						.as_deref()
						.is_some_and(|creator| creator != agent))
		})
	}

	fn answer_takeover(&mut self, pane: PaneId, agent: &str, approved: bool) {
		let requests = std::mem::take(&mut self.pending_takeovers);
		for request in requests {
			if request.args["id"].as_u64() != Some(pane as u64) || request.agent != agent {
				self.pending_takeovers.push(request);
				continue;
			}
			let result = if !request.is_active() {
				Err(String::from("接管请求已过期，没有执行"))
			} else if request
				.session
				.as_ref()
				.is_some_and(|session| self.session_agent(session).is_none())
			{
				Err(String::from("这个 even 成员正在停止或已经不存在"))
			} else if !approved {
				Err(String::from(
					"人拒绝了接管，没有执行；不要重试或改用其他窗口绕过拒绝。",
				))
			} else if self.pane(pane).is_none_or(|p| {
				p.owner != Owner::Normal && p.owner != Owner::Agent(agent.to_string())
			}) {
				Err(String::from("窗口已经关闭或归属已改变，没有执行"))
			} else {
				self.grant_agent(pane, agent)
					.and_then(|()| self.mcp_tool(&request.tool, &request.args, agent))
			};
			let _ = request.reply.send(result);
		}
	}

	fn parse_id(&self, args: &Value) -> Result<PaneId, String> {
		args["id"]
			.as_u64()
			.map(|v| v as PaneId)
			.ok_or_else(|| String::from("缺少 id"))
	}

	fn mcp_tool(&mut self, tool: &str, args: &Value, agent: &str) -> Result<Value, String> {
		match tool {
			"list_windows" => {
				let cols = (self.width / self.canvas.cell_w.max(1)).max(1);
				let rows =
					((self.height - self.canvas.cell_h).max(1) / self.canvas.cell_h.max(1)).max(1);
				let mut tabs = Vec::new();
				for (index, tab) in self.tabs.iter().enumerate() {
					let mut panes = Vec::new();
					let drawn: Vec<PaneId> = tab.saved.as_ref().unwrap_or(&tab.layout).leaves();
					for id in drawn.iter().copied().chain(tab.hidden.iter().copied()) {
						let Some(pane) = tab.panes.get(&id) else {
							continue;
						};
						let events: Vec<Value> = self
							.events
							.iter()
							.filter(|e| e.pane == id)
							.map(|e| json!({"id": e.id, "label": e.label}))
							.collect();
						panes.push(json!({
							"id": pane.id,
							"title": Self::pane_title(pane),
							"kind": Self::pane_kind(pane),
							"owner": owner_label(&pane.owner),
							"seen": pane.seen,
							"shown": drawn.contains(&id),
							"focused": pane.id == tab.focus,
							"events": events,
						}));
					}
					tabs.push(
						json!({"tab": index + 1, "active": index == self.active, "panes": panes}),
					);
				}
				Ok(
					json!({"text": serde_json::to_string_pretty(&json!({"cols": cols, "rows": rows, "tabs": tabs})).unwrap_or_default()}),
				)
			}
			"read_window" => {
				let id = self.parse_id(args)?;
				let pane = self.pane(id).ok_or_else(|| format!("没有窗口 {id}"))?;
				let text = match &pane.body {
					Body::Term(term) => term.snapshot(),
					Body::Solid(editor) => editor.text(),
					Body::Even(group) => group.transcript(),
				};
				Ok(json!({"text": text}))
			}
			"write_window" => {
				let id = self.parse_id(args)?;
				let text = args["text"]
					.as_str()
					.ok_or_else(|| String::from("缺少 text"))?;
				self.agent_access(id, agent)?;
				let Some(pane) = self.pane_mut(id) else {
					return Err(format!("没有窗口 {id}"));
				};
				let Body::Solid(editor) = &mut pane.body else {
					return Err(String::from("这个窗口不是 solid"));
				};
				editor.replace_all(text);
				editor
					.save()
					.map_err(|err| format!("窗口内容已更新但文件保存失败：{err}"))?;
				// 文件编辑事件绑在这个窗口上：事件还在，窗口就不能关。
				// 写是同步完成的；事件留一会儿，让人看见文件被改过。
				self.events
					.retain(|e| e.pane != id || e.kind != EventKind::AiEdit);
				let event_id = self.next_event;
				self.next_event += 1;
				self.events.push(EventRecord {
					id: event_id,
					pane: id,
					kind: EventKind::AiEdit,
					label: format!("{agent} 编辑了这个文件"),
					member: Some(agent.to_string()),
					expires: Some(Instant::now() + AI_EDIT_EVENT_TTL),
				});
				self.note_ai(id);
				Ok(
					json!({"text": format!("已写入窗口 {id} 并保存文件；事件 {event_id} 绑着这个窗口，稍后自动释放")}),
				)
			}
			"run_in_terminal" => {
				let id = self.parse_id(args)?;
				let command = args["command"]
					.as_str()
					.ok_or_else(|| String::from("缺少 command"))?;
				self.agent_access(id, agent)?;
				if let Some(event) = self.events.iter().find(|e| e.pane == id) {
					return Err(format!(
						"窗口 {id} 已经被事件 {} 绑着（{}），先 stop_event {}",
						event.id, event.label, event.id
					));
				}
				let Some(pane) = self.pane_mut(id) else {
					return Err(format!("没有窗口 {id}"));
				};
				let Body::Term(term) = &mut pane.body else {
					return Err(String::from("这个窗口不是 terminal"));
				};
				term.write(command.as_bytes());
				term.write(b"\r");
				let event_id = self.next_event;
				self.next_event += 1;
				self.events.push(EventRecord {
					id: event_id,
					pane: id,
					kind: EventKind::AiTask,
					label: format!("{agent} 正在窗口里运行：{command}"),
					member: Some(agent.to_string()),
					expires: None,
				});
				self.note_ai(id);
				Ok(
					json!({"text": format!("已在窗口 {id} 输入命令；事件 {event_id} 绑着这个窗口，用 stop_event {event_id} 释放")}),
				)
			}
			"create_window" => {
				let kind = args["kind"].as_str().unwrap_or("terminal");
				self.prune_ai_panes();
				let before = self.focused_id();
				let id = match kind {
					"solid" => {
						let path = args["path"]
							.as_str()
							.ok_or_else(|| String::from("solid 需要 path"))?;
						let path = PathBuf::from(path);
						if let Some(id) = self.find_solid(&path) {
							return Ok(json!({"text": format!("窗口 {id} 已经打开这个文件")}));
						}
						let editor = Editor::open(path, false)
							.map_err(|err| format!("文件没有打开：{err}"))?;
						let id = self.next_pane;
						self.next_pane += 1;
						self.add_tab(id, Body::Solid(editor), Owner::Normal, true, 0, None, false);
						Some(id)
					}
					"even" => {
						let group_id = self.next_group;
						self.next_group += 1;
						let name = format!("even{group_id}");
						let root = std::env::current_dir()
							.map(|p| p.to_string_lossy().into_owned())
							.unwrap_or_default();
						let group = GroupPane::new(&name, &root, self.client.clone());
						let id = self.next_pane;
						self.next_pane += 1;
						self.add_tab(
							id,
							Body::Even(Box::new(group)),
							Owner::Normal,
							true,
							group_id,
							None,
							false,
						);
						Some(id)
					}
					_ => {
						let command = args["command"].as_str().unwrap_or(&self.shell).to_string();
						let envs = self.child_env.clone();
						let (id, term) = self
							.spawn_term(&command, &envs)
							.ok_or_else(|| String::from("终端没有启动"))?;
						self.add_tab(id, Body::Term(term), Owner::Normal, true, 0, None, false);
						Some(id)
					}
				};
				if let Some(previous) = before {
					self.focus_pane(previous);
				}
				let id = id.ok_or_else(|| String::from("创建失败"))?;
				if let Some(pane) = self.pane_mut(id) {
					pane.creator = Some(agent.to_string());
					if let Some(title) = args["title"].as_str() {
						pane.title = Some(title.to_string());
					}
				}
				Ok(json!({"text": format!("已创建窗口 {id}（{kind}）")}))
			}
			"close_window" => {
				let id = self.parse_id(args)?;
				if self.events.iter().any(|e| e.pane == id) {
					let list: Vec<String> = self
						.events
						.iter()
						.filter(|e| e.pane == id)
						.map(|e| format!("#{} {}", e.id, e.label))
						.collect();
					return Err(format!("窗口 {id} 绑着事件，不能关：{}", list.join("；")));
				}
				let Some(pane) = self.pane(id) else {
					return Err(format!("没有窗口 {id}"));
				};
				if matches!(&pane.body, Body::Solid(editor) if editor.modified) {
					return Err(String::from("文件尚未保存，不能关闭"));
				}
				let owned = matches!(&pane.owner, Owner::Agent(a) if a == agent);
				let mine = pane.creator.as_deref() == Some(agent);
				if !owned && !mine && !self.auth.contains_key(&(id, agent.to_string())) {
					return Err(format!("窗口 {id} 不是你的；先申请接管"));
				}
				self.remove_pane_direct(id);
				Ok(json!({"text": format!("已关闭窗口 {id}")}))
			}
			"list_events" => {
				let events: Vec<Value> = self
					.events
					.iter()
					.map(|e| json!({"id": e.id, "pane": e.pane, "kind": "task", "label": e.label}))
					.collect();
				Ok(
					json!({"text": serde_json::to_string_pretty(&json!({"events": events})).unwrap_or_default()}),
				)
			}
			"stop_event" => {
				let id = self.parse_id(args).map(|v| v as u64)?;
				if !self.events.iter().any(|e| e.id == id) {
					return Err(format!("没有事件 {id}"));
				}
				let event = self.events.iter().find(|e| e.id == id).unwrap();
				let pane = self
					.pane(event.pane)
					.ok_or_else(|| String::from("窗口已关闭"))?;
				if !matches!(&pane.owner, Owner::Agent(owner) if owner == agent)
					&& event.member.as_deref() != Some(agent)
				{
					return Err(String::from("不能停止其他成员或人的任务"));
				}
				self.stop_event(id);
				Ok(
					json!({"text": format!("已请求停止事件 {id}；以 list_events 和 read_window 的实际结果为准")}),
				)
			}
			"takeover_status" => {
				let id = self.parse_id(args)?;
				let pane = self.pane(id).ok_or_else(|| format!("没有窗口 {id}"))?;
				let authorized = self.auth.contains_key(&(id, agent.to_string()));
				let owned = matches!(&pane.owner, Owner::Agent(a) if a == agent);
				let needs_request = pane.owner == Owner::Normal
					&& !authorized && (pane.seen
					|| (pane.creator.is_some() && pane.creator.as_deref() != Some(agent)));
				Ok(json!({"text": serde_json::to_string_pretty(&json!({
					"pane": id,
					"owner": owner_label(&pane.owner),
					"seen": pane.seen,
					"authorized": authorized,
					"owned": owned,
					"needs_request": needs_request,
				})).unwrap_or_default()}))
			}
			other => Err(format!("未知工具 {other}")),
		}
	}

	fn agent_access(&mut self, id: PaneId, agent: &str) -> Result<(), String> {
		let Some((owner, seen, creator)) = self
			.pane(id)
			.map(|p| (p.owner.clone(), p.seen, p.creator.clone()))
		else {
			return Err(format!("没有窗口 {id}"));
		};
		match &owner {
			// The human decides agent or share; the agent keeps working in either.
			Owner::Agent(owner) | Owner::Share(owner) if owner == agent => return Ok(()),
			Owner::Agent(owner) | Owner::Share(owner) => {
				return Err(format!("窗口 {id} 属于 {owner}"));
			}
			Owner::Normal => {}
		}
		let authorized = self.auth.contains_key(&(id, agent.to_string()));
		let mine = creator.as_deref() == Some(agent);
		let needs_request = seen || (creator.is_some() && !mine);
		if !needs_request || authorized {
			return self.grant_agent(id, agent);
		}
		Err(format!("接管窗口 {id} 需要人批准，没有执行。"))
	}

	fn grant_agent(&mut self, id: PaneId, agent: &str) -> Result<(), String> {
		let pane = self.pane_mut(id).ok_or_else(|| format!("没有窗口 {id}"))?;
		if let Body::Solid(editor) = &mut pane.body {
			if editor.modified {
				editor
					.save()
					.map_err(|err| format!("接管前保存失败，没有交给 AI：{err}"))?;
			}
		}
		pane.owner = Owner::Agent(agent.to_string());
		pane.seen = false;
		self.auth.insert((id, agent.to_string()), ());
		Ok(())
	}

	fn prune_ai_panes(&mut self) {
		let mut ids: Vec<PaneId> = self
			.tabs
			.iter()
			.flat_map(|t| t.panes.values())
			.filter(|p| {
				p.ai_created
					&& p.owner == Owner::Normal
					&& !self.events.iter().any(|e| e.pane == p.id)
					&& match &p.body {
						Body::Solid(editor) => !editor.ever_modified(),
						Body::Term(_) | Body::Even(_) => false,
					}
			})
			.map(|p| p.id)
			.collect();
		ids.sort_unstable();
		if ids.len() >= 5 {
			self.remove_pane_direct(ids[0]);
		}
	}

	fn modal_key(&mut self, key: &Key) {
		if key.vk == VK_ESCAPE {
			if let Some(Modal::Takeover { pane, agent }) = self.modal.take() {
				self.answer_takeover(pane, &agent, false);
			} else {
				self.modal = None;
			}
			return;
		}
		let modal = self.modal.take();
		match modal {
			Some(Modal::Help) => {}
			Some(Modal::Notice(_)) => {}
			Some(Modal::Tabs) => match key.ch {
				Some(ch @ '1'..='9') => {
					let index = (ch as u8 - b'1') as usize;
					if index < self.tabs.len() {
						self.active = index;
						let focus = self.tabs[index].focus;
						self.note_human(focus);
						self.mark_seen();
					}
				}
				_ => self.modal = Some(Modal::Tabs),
			},
			Some(Modal::Events { pane }) => {
				if key.ch == Some('s') {
					self.stop_events_for(pane);
				} else {
					self.modal = Some(Modal::Events { pane });
				}
			}
			Some(Modal::Takeover { pane, agent }) => match key.ch {
				Some('a') => self.answer_takeover(pane, &agent, true),
				Some('r') => self.answer_takeover(pane, &agent, false),
				_ => self.modal = Some(Modal::Takeover { pane, agent }),
			},
			Some(Modal::CloseAgent { pane, agent }) => {
				if key.ch == Some('s') {
					self.stop_events_for(pane);
				} else {
					self.modal = Some(Modal::CloseAgent { pane, agent });
				}
			}
			Some(Modal::PaneLimit { scroll }) => match key.ch {
				Some(ch @ '1'..='6') => self.set_pane_limit((ch as u8 - b'0') as usize),
				_ => self.modal = Some(Modal::PaneLimit { scroll }),
			},
			None => {}
		}
	}

	fn modal_lines(&self) -> (String, Vec<String>) {
		match &self.modal {
			Some(Modal::Help) => {
				let mcp = self
					.mcp
					.as_ref()
					.map(|m| m.url.clone())
					.unwrap_or_else(|| String::from("(MCP 没有起来)"));
				(
					String::from("Delta 帮助"),
					vec![
						String::from("普通 / 输入：F1 回普通（AI 窗口回 agent）"),
						String::from("普通（agent 同）：所有键归 Delta，不送给程序"),
						String::from(
							"  c 新标签  x 关窗口  & 关标签  z 缩放  方向键换窗口  q 窗口编号",
						),
						String::from(
							"  n 下一个标签  p 窗口列表 / 同屏数  1-9 选标签  w 标签列表  [ 滚屏模式",
						),
						String::from("  ! 拆成新标签  ? 帮助  i 进输入（AI 窗口进 share）"),
						String::from("  o odd  e even  s 选文件开 solid"),
						String::from("  Ctrl+方向键调大小  Ctrl+= / Ctrl+- / Ctrl+滚轮 放大缩小窗口字体"),
						String::from("  Ctrl+C 复制选区  Ctrl+V 粘贴"),
						String::from(
							"输入（share 同）：只有 F1-F12 留在 Delta（F1 回普通，F2-F12 暂无作用），",
						),
						String::from("  其余按键和鼠标原样交给程序"),
						String::from("  solid：Ctrl+A 全选  Ctrl+C 复制  Ctrl+X 剪切  Ctrl+V 粘贴"),
						String::from(
							"        Ctrl+Z 撤销  Ctrl+Shift+Z / Ctrl+Y 重做  Ctrl+S 保存",
						),
						String::from(
							"  even：Ctrl+A 全选  Ctrl+C 复制  Ctrl+X 剪切（只剪输入行）  Ctrl+V 粘贴",
						),
						String::from("       Ctrl+Z / Ctrl+Y 撤销、重做"),
						String::from(
							"       粘贴图片写成 ![](临时路径)；Ctrl+单击图片路径或网址用系统默认程序打开",
						),
						String::from("  终端：Ctrl+C 等由前台程序自己决定；选字和滚历史回普通模式"),
						String::from("鼠标：普通里拖动选字、滚轮看历史、Ctrl+滚轮缩放窗口；输入里拖动、滚轮交给程序"),
						String::from(
							"接管：读窗口不用申请；写入/接管要窗口归属或人批准；批准会记住",
						),
						String::from("关闭：有任务事件时拒绝并列出；AI 的窗口先停止任务"),
						String::from(
							"odd：默认终端就是 odd 本身；solid：终端里 \"$SOLID\" <文件> 或启动时带 <文件>",
						),
						format!("Delta MCP：{mcp}"),
					],
				)
			}
			Some(Modal::Events { pane }) => {
				let mut lines = vec![format!("窗口 {pane} 绑着这些事件：")];
				for event in self.events.iter().filter(|e| e.pane == *pane) {
					lines.push(format!("#{}  {}", event.id, event.label));
				}
				lines.push(String::from("s 停止这些事件；Esc 关闭本框"));
				(String::from("不能关闭"), lines)
			}
			Some(Modal::Takeover { pane, agent }) => (
				String::from("接管请求"),
				vec![
					format!("{} 想接管窗口 {pane}。", agent_badge(agent)),
					String::from("a 同意；r 拒绝；Esc 拒绝"),
					String::from("同意后自动继续；你再次在前台输入过，下一次接管会重新申请。"),
				],
			),
			Some(Modal::CloseAgent { pane, agent }) => (
				String::from("AI 的窗口"),
				vec![
					format!("窗口 {pane} 属于 {}。", agent_badge(agent)),
					String::from("s 停止它的任务（窗口留给你）"),
					String::from("Esc 关闭本框"),
				],
			),
			Some(Modal::Tabs) => {
				let mut lines = Vec::new();
				for index in 0..self.tabs.len() {
					lines.push(format!("{}. {}", index + 1, self.tab_title(index)));
				}
				lines.push(String::from("按 1-9 切换；Esc 关闭"));
				(String::from("标签"), lines)
			}
			Some(Modal::PaneLimit { .. }) => {
				let marks: Vec<String> = (1..=6)
					.map(|n| {
						if n == self.pane_limit {
							format!("[{n}]")
						} else {
							format!(" {n} ")
						}
					})
					.collect();
				let mut lines = vec![
					marks.join(" "),
					format!(
						"最多显示 {} 个；按 1-6 设置；滚轮查看下列窗口；Esc 关闭",
						self.pane_limit
					),
					format!("  #   {} 状态  名称", pad_text("类型", 9)),
				];
				if let Some(tab) = self.tabs.get(self.active) {
					let drawn = Self::drawn_ids(tab);
					let mut ids = drawn.clone();
					ids.extend(tab.hidden.iter().copied().filter(|id| !drawn.contains(id)));
					for id in ids {
						let Some(pane) = tab.panes.get(&id) else {
							continue;
						};
						let mark = if id == tab.focus { ">" } else { " " };
						let state = if drawn.contains(&id) {
							"显示"
						} else {
							"隐藏"
						};
						let mut row = format!(
							"{mark}{id:>3}  {} {state}  {}",
							pad_text(Self::pane_kind(pane), 9),
							Self::pane_title(pane)
						);
						if !matches!(pane.owner, Owner::Normal) {
							row.push_str("  ");
							row.push_str(&owner_label(&pane.owner));
						}
						lines.push(row);
					}
				}
				(String::from("窗口"), lines)
			}
			Some(Modal::Notice(text)) => (
				String::from("提示"),
				text.lines().map(String::from).collect(),
			),
			None => (String::new(), Vec::new()),
		}
	}

	fn tab_title(&self, index: usize) -> String {
		let Some(tab) = self.tabs.get(index) else {
			return String::new();
		};
		let Some(pane) = tab.panes.get(&tab.focus) else {
			return String::new();
		};
		clip_text(&Self::pane_title(pane), 18)
	}

	fn paint(&mut self) {
		self.canvas.select_font(canvas::FONT_SIZE);
		let cw = self.canvas.cell_w;
		let ch = self.canvas.cell_h;
		let area = self.content_area();
		self.canvas.clear();
		self.canvas
			.fill(0, 0, self.width, self.height, canvas::rgb(28, 30, 40));
		let active = self.active;
		let drawn = self
			.tabs
			.get(active)
			.map(|tab| (self.rects_of(tab, area), tab.focus));
		if let Some((rects, focus)) = drawn {
			let show_cursor = self.mode != Mode::Normal && self.modal.is_none();
			let canvas = &mut self.canvas;
			let tab = &mut self.tabs[active];
			for (id, rect) in &rects {
				let Some(pane) = tab.panes.get_mut(id) else {
					continue;
				};
				// Window content uses its own point size; the geometry above stays
				// on the base grid so Ctrl+= grows one window without moving borders.
				canvas.select_font(pane.font);
				let focused = *id == focus;
				match &mut pane.body {
					Body::Term(term) => term.render(canvas, *rect, focused, show_cursor, None),
					Body::Solid(editor) => editor.render(canvas, *rect, focused, show_cursor),
					Body::Even(group) => group.render(canvas, *rect, focused),
				}
			}
			canvas.select_font(canvas::FONT_SIZE);
			for (id, rect) in &rects {
				let Some(pane) = tab.panes.get(id) else {
					continue;
				};
				let focused = *id == focus;
				// A pane an AI is holding keeps its owner colour whether or not it is
				// focused; the inner line carries the focus instead.
				let held_by_ai = matches!(&pane.owner, Owner::Agent(_) | Owner::Share(_));
				let color = if focused && !held_by_ai {
					canvas::rgb(
						canvas::BORDER_ACTIVE.0,
						canvas::BORDER_ACTIVE.1,
						canvas::BORDER_ACTIVE.2,
					)
				} else {
					owner_color(&pane.owner)
				};
				if held_by_ai {
					// A 1-pixel line disappears on the dark background; an unfocused AI
					// pane must still show whose it is, so the owner band is 3 pixels.
					canvas.fill(rect.left, rect.top, rect.right, rect.top + 3, color);
					canvas.fill(rect.left, rect.bottom - 3, rect.right, rect.bottom, color);
					canvas.fill(rect.left, rect.top, rect.left + 3, rect.bottom, color);
					canvas.fill(rect.right - 3, rect.top, rect.right, rect.bottom, color);
				} else {
					canvas.outline(rect.left, rect.top, rect.right - 1, rect.bottom - 1, color);
				}
				if focused && held_by_ai {
					// The owner band says who holds the pane, the inner line says it still
					// has the keyboard focus.
					let accent = canvas::rgb(
						canvas::BORDER_ACTIVE.0,
						canvas::BORDER_ACTIVE.1,
						canvas::BORDER_ACTIVE.2,
					);
					canvas.outline(
						rect.left + 4,
						rect.top + 4,
						rect.right - 5,
						rect.bottom - 5,
						accent,
					);
				}
				if self.show_numbers {
					let label = format!(" {} ", id);
					let width = label.len() as i32 * cw;
					canvas.fill(
						rect.left,
						rect.top,
						rect.left + width,
						rect.top + ch,
						canvas::rgb(60, 70, 90),
					);
					canvas.text(rect.left, rect.top, &label, canvas::rgb(240, 240, 240));
				}
				if held_by_ai {
					let label = match &pane.owner {
						Owner::Agent(agent) => format!(" agent {} ", agent_badge(agent)),
						Owner::Share(agent) => format!(" share {} ", agent_badge(agent)),
						Owner::Normal => String::new(),
					};
					let room = ((rect.right - rect.left) / cw.max(1)).max(1) as usize;
					let label = clip_text(&label, room);
					let width = label.width() as i32 * cw;
					let x = (rect.right - width).max(rect.left);
					canvas.fill(x, rect.top, rect.right, rect.top + ch, color);
					canvas.text(x, rect.top, &label, canvas::rgb(20, 16, 26));
				}
			}
		}
		if let Some((x, y)) = self.ime_caret {
			if !self.ime_text.is_empty() {
				let text = clip_text(&self.ime_text, 40);
				let width = text.width() as i32 * cw;
				self.canvas
					.fill(x, y, x + width, y + ch, canvas::rgb(60, 52, 30));
				self.canvas.text(x, y, &text, canvas::rgb(240, 220, 170));
			}
		}
		self.draw_status();
		self.draw_modal();
		let hwnd = self.hwnd;
		self.canvas.present(hwnd);
	}

	fn draw_status(&mut self) {
		let cw = self.canvas.cell_w;
		let ch = self.canvas.cell_h;
		let y = self.height - ch;
		self.canvas.fill(
			0,
			y,
			self.width,
			self.height,
			canvas::rgb(
				canvas::STATUS_BG.0,
				canvas::STATUS_BG.1,
				canvas::STATUS_BG.2,
			),
		);
		let mut x = 0i32;
		for index in 0..self.tabs.len() {
			let label = format!(" {} {} ", index + 1, self.tab_title(index));
			let width = label.width() as i32 * cw;
			if x + width > self.width - 260 {
				break;
			}
			if index == self.active {
				self.canvas.fill(
					x,
					y,
					x + width,
					y + ch,
					canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
				);
				self.canvas.text(x, y, &label, canvas::rgb(10, 12, 16));
			} else {
				self.canvas.text(
					x,
					y,
					&label,
					canvas::rgb(
						canvas::STATUS_FG.0,
						canvas::STATUS_FG.1,
						canvas::STATUS_FG.2,
					),
				);
			}
			x += width;
		}
		let pane = self.focused();
		// The mode field holds one of four window modes: normal, insert, agent,
		// share. Agent and share belong to the AI and carry its name.
		let owner_mode = pane.and_then(|p| match &p.owner {
			Owner::Agent(agent) => Some((
				format!("agent {}", agent_badge(agent)),
				owner_color(&p.owner),
			)),
			Owner::Share(agent) => Some((
				format!("share {}", agent_badge(agent)),
				owner_color(&p.owner),
			)),
			Owner::Normal => None,
		});
		let (mode_label, mode_color) = match (owner_mode, self.scroll_mode) {
			(Some((label, color)), _) => (label, Some(color)),
			(None, true) => (String::from("SCROLL"), None),
			(None, false) => (
				String::from(match self.mode {
					Mode::Normal => "NORMAL",
					Mode::Insert => "INSERT",
				}),
				None,
			),
		};
		let zoom = self
			.tabs
			.get(self.active)
			.map(|t| t.saved.is_some())
			.unwrap_or(false);
		let prefix = format!("{mode_label}  ");
		let tail = format!(
			"tab {}/{}  pane {}{} ",
			self.active + 1,
			self.tabs.len(),
			self.focused_id().unwrap_or(0),
			if zoom { "  ZOOM" } else { "" }
		);
		let right = format!("{prefix}{tail}");
		let width = right.width() as i32 * cw;
		let x = (self.width - width - 4).max(0);
		let accent = canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2);
		self.canvas.text(x, y, &right, accent);
		if let Some(color) = mode_color {
			let label_w = mode_label.width() as i32 * cw;
			self.canvas.fill(x, y, x + label_w, y + ch, color);
			self.canvas.text(x, y, &mode_label, canvas::rgb(20, 16, 26));
		}
	}

	fn draw_modal(&mut self) {
		if let Some(Modal::PaneLimit { scroll }) = &self.modal {
			let scroll = *scroll;
			self.draw_pane_limit(scroll);
			return;
		}
		let (title, lines) = self.modal_lines();
		if title.is_empty() && lines.is_empty() {
			return;
		}
		let cw = self.canvas.cell_w;
		let ch = self.canvas.cell_h;
		let mut width = title.width();
		for line in &lines {
			width = width.max(line.width());
		}
		width = width.min(((self.width / cw) as usize).saturating_sub(6).max(20));
		let mut wrapped = Vec::new();
		for line in &lines {
			wrapped.extend(wrap_text(line, width));
		}
		let box_w = ((width + 4) as i32 * cw).min(self.width - 20);
		let box_h = ((wrapped.len() + 3) as i32 * ch).min(self.height - 20);
		let x = (self.width - box_w) / 2;
		let y = (self.height - box_h) / 2;
		self.canvas
			.fill(x, y, x + box_w, y + box_h, canvas::rgb(20, 22, 30));
		self.canvas.outline(
			x,
			y,
			x + box_w,
			y + box_h,
			canvas::rgb(
				canvas::BORDER_ACTIVE.0,
				canvas::BORDER_ACTIVE.1,
				canvas::BORDER_ACTIVE.2,
			),
		);
		self.canvas.text(
			x + cw,
			y + ch,
			&clip_text(&title, width),
			canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
		);
		for (index, line) in wrapped.iter().enumerate() {
			self.canvas.text(
				x + cw,
				y + (2 + index as i32) * ch,
				&clip_text(line, width),
				canvas::rgb(
					canvas::DEFAULT_FG.0,
					canvas::DEFAULT_FG.1,
					canvas::DEFAULT_FG.2,
				),
			);
		}
	}

	/// The `p` popup: a fixed-height window list; the wheel moves the list, the
	/// header rows stay where they are.
	fn draw_pane_limit(&mut self, scroll: isize) {
		const FIXED: usize = 3;
		let (title, lines) = self.modal_lines();
		let fixed = FIXED.min(lines.len());
		let list = &lines[fixed..];
		let start = (scroll.max(0) as usize).min(list.len().saturating_sub(PANE_LIST_ROWS));
		let cw = self.canvas.cell_w;
		let ch = self.canvas.cell_h;
		let mut width = title.width();
		for line in &lines {
			width = width.max(line.width());
		}
		width = width.min(((self.width / cw) as usize).saturating_sub(6).max(20));
		let box_w = ((width + 4) as i32 * cw).min(self.width - 20);
		let box_h = (((3 + fixed + PANE_LIST_ROWS) as i32) * ch).min(self.height - 20);
		let x = (self.width - box_w) / 2;
		let y = (self.height - box_h) / 2;
		self.canvas
			.fill(x, y, x + box_w, y + box_h, canvas::rgb(20, 22, 30));
		self.canvas.outline(
			x,
			y,
			x + box_w,
			y + box_h,
			canvas::rgb(
				canvas::BORDER_ACTIVE.0,
				canvas::BORDER_ACTIVE.1,
				canvas::BORDER_ACTIVE.2,
			),
		);
		self.canvas.text(
			x + cw,
			y + ch,
			&clip_text(&title, width),
			canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
		);
		let fg = canvas::rgb(
			canvas::DEFAULT_FG.0,
			canvas::DEFAULT_FG.1,
			canvas::DEFAULT_FG.2,
		);
		for (index, line) in lines[..fixed].iter().enumerate() {
			self.canvas.text(
				x + cw,
				y + (2 + index as i32) * ch,
				&clip_text(line, width),
				fg,
			);
		}
		// Fixed rows even when the list is shorter, so the box does not jump.
		for row in 0..PANE_LIST_ROWS {
			let Some(line) = list.get(start + row) else {
				break;
			};
			self.canvas.text(
				x + cw,
				y + (2 + fixed + row) as i32 * ch,
				&clip_text(line, width),
				fg,
			);
		}
		if list.len() > PANE_LIST_ROWS {
			let more = format!("{}/{}  滚轮查看", start + 1, list.len());
			let w = more.width() as i32 * cw;
			self.canvas.text(
				x + box_w - w - cw,
				y + box_h - ch,
				&more,
				canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
			);
		}
	}
}

unsafe fn forward_open(hwnd: HWND, path: &std::path::Path) -> bool {
	let wide = canvas::wide(&path.to_string_lossy());
	let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, (wide.len() - 1) * 2);
	let data = COPYDATASTRUCT {
		dwData: COPYDATA_TAG,
		cbData: bytes.len() as u32,
		lpData: bytes.as_ptr() as *mut _,
	};
	SendMessageW(
		hwnd,
		WM_COPYDATA,
		0,
		&data as *const COPYDATASTRUCT as isize,
	) != 0
}

fn default_shell() -> String {
	std::env::current_exe()
		.ok()
		.and_then(|exe| {
			exe.parent()
				.map(|dir| format!("\"{}\"", dir.join("odd.exe").display()))
		})
		.unwrap_or_else(|| String::from("odd.exe"))
}

fn resize_body(body: &mut Body, rect: &Rect, cw: i32, ch: i32) {
	if let Body::Term(term) = body {
		let cols = (rect.width() / cw).max(1) as usize;
		let rows = (rect.height() / ch).max(1) as usize;
		term.resize(cols, rows);
	}
}

fn scroll_body(body: &mut Body, delta: i32) {
	match body {
		Body::Term(term) => {
			if delta >= 0 {
				term.scroll_up(delta as usize);
			} else {
				term.scroll_down((-delta) as usize);
			}
		}
		Body::Solid(editor) => {
			editor.move_cursor(-delta, 0, false);
		}
		Body::Even(group) => group.scroll(delta),
	}
}

fn priority_key(prio: &HashMap<PaneId, Priority>, id: PaneId) -> (u64, u64, PaneId) {
	let entry = prio.get(&id).copied().unwrap_or_default();
	(entry.human, entry.ai, id)
}

fn highest_priority(ids: &[PaneId], prio: &HashMap<PaneId, Priority>) -> Option<PaneId> {
	ids.iter().copied().max_by_key(|id| priority_key(prio, *id))
}

fn lowest_priority(
	ids: &[PaneId],
	prio: &HashMap<PaneId, Priority>,
	except: Option<PaneId>,
) -> Option<PaneId> {
	ids.iter()
		.copied()
		.filter(|id| Some(*id) != except)
		.min_by_key(|id| priority_key(prio, *id))
}

fn owner_label(owner: &Owner) -> String {
	match owner {
		Owner::Normal => String::from("normal"),
		Owner::Agent(agent) => format!("agent:{agent}"),
		Owner::Share(agent) => format!("share:{agent}"),
	}
}

/// `even3:sam` reads `3-sam` on screen.
fn agent_badge(agent: &str) -> String {
	match agent.split_once(':') {
		Some((group, name)) => format!("{}-{name}", group.strip_prefix("even").unwrap_or(group)),
		None => String::from(agent),
	}
}

fn owner_color(owner: &Owner) -> u32 {
	match owner {
		Owner::Normal => canvas::rgb(canvas::BORDER.0, canvas::BORDER.1, canvas::BORDER.2),
		Owner::Agent(_) => canvas::rgb(150, 110, 200),
		Owner::Share(_) => canvas::rgb(110, 180, 130),
	}
}

fn pad_text(text: &str, cols: usize) -> String {
	let width = text.width();
	if width >= cols {
		return text.to_string();
	}
	format!("{text}{}", " ".repeat(cols - width))
}

fn clip_text(text: &str, cols: usize) -> String {
	let mut out = String::new();
	let mut width = 0usize;
	for ch in text.chars() {
		let w = ch.width().unwrap_or(1);
		if width + w > cols {
			break;
		}
		out.push(ch);
		width += w;
	}
	out
}

/// Break a line into pieces that fit the box. Prefer the last space so a
/// wrapped command stays joinable by eye; fall back to a hard break for CJK.
fn wrap_text(text: &str, cols: usize) -> Vec<String> {
	let cols = cols.max(1);
	let mut out = Vec::new();
	let mut current = String::new();
	let mut width = 0usize;
	let mut last_space: Option<(usize, usize)> = None; // (char index, width before that space)
	for ch in text.chars() {
		let w = ch.width().unwrap_or(1);
		if width + w > cols {
			if let Some((index, _)) = last_space {
				let rest: String = current.chars().skip(index + 1).collect();
				current.truncate(
					current
						.char_indices()
						.nth(index)
						.map(|(i, _)| i)
						.unwrap_or(current.len()),
				);
				out.push(current.trim_end().to_string());
				current = rest;
				width = current.width();
			} else {
				out.push(std::mem::take(&mut current));
				width = 0;
			}
			last_space = None;
		}
		if ch == ' ' {
			last_space = Some((current.chars().count(), width));
		}
		current.push(ch);
		width += w;
	}
	if !current.is_empty() || out.is_empty() {
		out.push(current);
	}
	out
}

fn point_of(lparam: LPARAM) -> (i32, i32) {
	let x = (lparam & 0xffff) as u16 as i16 as i32;
	let y = ((lparam >> 16) & 0xffff) as u16 as i16 as i32;
	(x, y)
}

fn make_key(vk: u16, lparam: LPARAM, down: bool) -> Key {
	let mut key = Key::default();
	key.vk = vk;
	key.scan = ((lparam >> 16) & 0xff) as u16;
	key.enhanced = lparam & (1 << 24) != 0;
	key.down = down;
	key.repeat = 1;
	unsafe {
		key.shift = GetKeyState(VK_SHIFT as i32) < 0;
		key.ctrl = GetKeyState(VK_CONTROL as i32) < 0;
		key.alt = GetKeyState(VK_MENU as i32) < 0;
		key.right_alt = GetKeyState(VK_RMENU as i32) < 0;
		key.right_ctrl = GetKeyState(VK_RCONTROL as i32) < 0;
		key.caps = GetKeyState(VK_CAPITAL as i32) & 1 != 0;
		key.num = GetKeyState(VK_NUMLOCK as i32) & 1 != 0;
	}
	if down {
		key.ch = char_for_key(vk, key.scan);
	}
	key
}

fn char_for_key(vk: u16, scan: u16) -> Option<char> {
	unsafe {
		let mut state = [0u8; 256];
		if GetKeyboardState(state.as_mut_ptr()) == 0 {
			return None;
		}
		let mut buf = [0u16; 8];
		let layout = GetKeyboardLayout(0);
		let ret = ToUnicodeEx(
			vk as u32,
			scan as u32,
			state.as_ptr(),
			buf.as_mut_ptr(),
			buf.len() as i32,
			0,
			layout,
		);
		if ret > 0 {
			char::decode_utf16(buf[..ret as usize].iter().copied())
				.next()
				.and_then(|r| r.ok())
		} else {
			None
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn later_human_switch_comes_first() {
		let mut prio = HashMap::new();
		prio.insert(1, Priority { human: 3, ai: 9 });
		prio.insert(2, Priority { human: 8, ai: 0 });
		assert_eq!(lowest_priority(&[1, 2], &prio, None), Some(1));
		assert_eq!(highest_priority(&[1, 2], &prio), Some(2));
	}

	#[test]
	fn equal_human_times_fall_back_to_the_ai_time() {
		let mut prio = HashMap::new();
		prio.insert(4, Priority { human: 5, ai: 1 });
		prio.insert(9, Priority { human: 5, ai: 7 });
		assert_eq!(highest_priority(&[4, 9], &prio), Some(9));
		assert_eq!(lowest_priority(&[9, 4], &prio, None), Some(4));
	}

	#[test]
	fn identical_times_fall_back_to_the_window_number() {
		let mut prio = HashMap::new();
		prio.insert(3, Priority::default());
		prio.insert(6, Priority::default());
		assert_eq!(highest_priority(&[3, 6], &prio), Some(6));
		assert_eq!(lowest_priority(&[6, 3], &prio, None), Some(3));
	}

	#[test]
	fn the_focused_window_is_not_the_one_made_room_with() {
		let mut prio = HashMap::new();
		prio.insert(1, Priority { human: 1, ai: 0 });
		prio.insert(2, Priority { human: 2, ai: 0 });
		prio.insert(3, Priority { human: 3, ai: 0 });
		assert_eq!(lowest_priority(&[1, 2, 3], &prio, Some(1)), Some(2));
		// When the oldest is the focused one, the next oldest steps out.
		assert_eq!(lowest_priority(&[1, 2, 3], &prio, Some(2)), Some(1));
		// A tab drawing a single window has to give that one up.
		assert_eq!(lowest_priority(&[1], &prio, Some(1)), None);
		assert_eq!(lowest_priority(&[1], &prio, None), Some(1));
	}
}
