//! solid: our editor. Capability shape from fresh/helix/nvim (buffer, selection,
//! undo, tree-sitter highlighting); no mode machine and no borrowed keys.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tree_sitter::Parser;
use unicode_width::UnicodeWidthChar;

use crate::canvas::{self, Canvas};
use crate::clipboard;
use crate::input::Key;
use crate::langconf::{self, ScopeKind};
use crate::layout::Rect;
use crate::lsp;

/// `editor.quickSuggestionsDelay` default in the VS Code build we follow.
const QUICK_SUGGEST_DELAY_MS: u64 = 10;

/// `Number(word)` as `shouldAutoTrigger` reads it.
fn is_js_number(word: &str) -> bool {
	let lower = word.to_ascii_lowercase();
	if let Some(rest) = lower.strip_prefix("0x") {
		return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_hexdigit());
	}
	if let Some(rest) = lower.strip_prefix("0b") {
		return !rest.is_empty() && rest.chars().all(|c| c == '0' || c == '1');
	}
	if let Some(rest) = lower.strip_prefix("0o") {
		return !rest.is_empty() && rest.chars().all(|c| ('0'..='7').contains(&c));
	}
	word.parse::<f64>().is_ok()
}

/// Catppuccin Mocha syntax groups, taken from the reference colorscheme
/// (`vendor/catppuccin-nvim-2.0.0/lua/catppuccin/groups/syntax.lua` and
/// `treesitter.lua`). The TextMate scope each variant stands for is written on
/// the variant and resolved in `hl_color`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hl {
	Plain,
	Comment,
	String,
	Regex,
	Number,
	/// storage / keyword / keyword.other / constant.language / meta.preprocessor
	Keyword,
	/// keyword.control
	Control,
	/// entity.name.function / support.function
	Function,
	/// entity.name.type / support.type / entity.name.class / entity.name.namespace
	Type,
	/// variable.other.constant / variable.other.enummember
	Constant,
	/// variable / support.type.property-name / entity.other.attribute-name
	Variable,
	/// @variable.parameter
	Param,
	/// @variable.member / @property
	Member,
	/// @operator
	Operator,
	/// @punctuation.bracket / @punctuation.delimiter
	Punct,
	/// @variable.builtin
	Builtin,
	/// constant.character.escape
	Escape,
	/// entity.name.label
	Label,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
	Rust,
	C,
	Python,
	JavaScript,
	Json,
	Plain,
}

impl Lang {
	fn from_path(path: &std::path::Path) -> Self {
		match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
			"rs" => Lang::Rust,
			"c" | "h" => Lang::C,
			"py" => Lang::Python,
			"js" | "jsx" | "mjs" => Lang::JavaScript,
			"json" => Lang::Json,
			_ => Lang::Plain,
		}
	}

	fn language(self) -> Option<tree_sitter::Language> {
		Some(match self {
			Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
			Lang::C => tree_sitter_c::LANGUAGE.into(),
			Lang::Python => tree_sitter_python::LANGUAGE.into(),
			Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
			Lang::Json => tree_sitter_json::LANGUAGE.into(),
			Lang::Plain => return None,
		})
	}

	pub fn name(self) -> &'static str {
		match self {
			Lang::Rust => "rust",
			Lang::C => "c",
			Lang::Python => "python",
			Lang::JavaScript => "javascript",
			Lang::Json => "json",
			Lang::Plain => "text",
		}
	}
}

/// VS Code 的 auto-closed-character 记录：一个自动闭合动作记下括号对外沿，
/// 光标不在此范围内时它就被作废（_validateAutoClosedActions 的 isValid）。
#[derive(Clone, Copy)]
struct AutoClosed {
	open: (usize, usize),
	close: (usize, usize),
	close_len: usize,
}

/// The completion popup. Items come from `textDocument/completion`; the index
/// is a position in the filtered list, as Neovim's pum works on matches.
struct Completion {
	req: u64,
	items: Vec<lsp::Item>,
	index: usize,
	incomplete: bool,
	done: bool,
	/// Start of the typed word, (row, char column).
	start: (usize, usize),
}

/// An active snippet session: tab stops as absolute character offsets into
/// the document, `index` the stop the cursor sits on. Tab and Shift+Tab walk
/// the stops; leaving the snippet or pressing Esc ends the session.
struct Snippet {
	stops: Vec<(usize, usize)>,
	index: usize,
	/// Identifies the session so an accept inside a previous one does not
	/// shift the new stops by that edit.
	id: u64,
}

/// A git change marker in the gutter, the sign gitsigns draws: added lines
/// green, changed lines yellow, a deletion a red bar on the line above it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GitSign {
	Add,
	Change,
	Delete,
}

/// The file's repo and the signs for the current text, refreshed from
/// `git diff` on a debounce.
struct GitWatch {
	root: PathBuf,
	rel: String,
	signs: Vec<Option<GitSign>>,
}

/// An in-flight `textDocument/signatureHelp` and its answer. Blink shows the
/// window above the cursor while the call is being typed and hides it when
/// insert ends; `range` is the active parameter inside `help`'s label.
struct Signature {
	req: u64,
	done: bool,
	help: Option<lsp::SigHelp>,
	range: Option<(usize, usize)>,
}

pub struct Editor {
	pub path: PathBuf,
	pub lines: Vec<Vec<char>>,
	pub cursor: (usize, usize),
	pub anchor: Option<(usize, usize)>,
	pub scroll: usize,
	pub hscroll: usize,
	pub modified: bool,
	pub lang: Lang,
	pub status: String,
	pub read_only: bool,
	undo: Vec<(Vec<Vec<char>>, (usize, usize))>,
	redo: Vec<(Vec<Vec<char>>, (usize, usize))>,
	hl: Vec<Vec<Hl>>,
	hl_dirty: bool,
	column_goal: Option<usize>,
	scopes: Vec<Vec<ScopeKind>>,
	/// 自动插入的闭合符，对应 VS Code 的 auto-closed-character。
	auto_closed: Vec<AutoClosed>,
	lsp: Option<lsp::Doc>,
	/// Debounce for didChange: 150ms after the last edit.
	lsp_edit_at: Option<Instant>,
	completion: Option<Completion>,
	/// VS Code `quickSuggestionsDelay` timer: request completion this late
	/// after a keystroke unless another key lands first.
	suggest_at: Option<Instant>,
	suggest_pos: (usize, usize),
	snippet: Option<Snippet>,
	snippet_gen: u64,
	diags: Vec<lsp::Diag>,
	diag_gen: u64,
	git: Option<GitWatch>,
	/// Recompute the git signs once the user stops typing.
	git_at: Option<Instant>,
	sig: Option<Signature>,
	/// 按住方向键时光标加速：同一个键每重复一次多走一格，最多三格。
	repeat_vk: u16,
	repeat_steps: u8,
}

impl Editor {
	pub fn open(path: PathBuf, read_only: bool) -> std::io::Result<Self> {
		let content = match std::fs::read_to_string(&path) {
			Ok(content) => content,
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
			Err(err) => return Err(err),
		};
		Ok(Self::new(path, content, read_only))
	}

	pub fn new(path: PathBuf, content: String, read_only: bool) -> Self {
		let mut lines: Vec<Vec<char>> = content.split('\n').map(|l| l.chars().collect()).collect();
		if lines.is_empty() {
			lines.push(Vec::new());
		}
		if lines.iter().all(|l| l.is_empty()) {
			lines.clear();
			lines.push(Vec::new());
		}
		let lang = Lang::from_path(&path);
		let mut ed = Self {
			path,
			lines,
			cursor: (0, 0),
			anchor: None,
			scroll: 0,
			hscroll: 0,
			modified: false,
			lang,
			status: String::new(),
			read_only,
			undo: Vec::new(),
			redo: Vec::new(),
			hl: Vec::new(),
			hl_dirty: true,
			column_goal: None,
			scopes: Vec::new(),
			auto_closed: Vec::new(),
			lsp: None,
			lsp_edit_at: None,
			completion: None,
			suggest_at: None,
			suggest_pos: (0, 0),
			snippet: None,
			snippet_gen: 0,
			diags: Vec::new(),
			diag_gen: 0,
			git: None,
			git_at: None,
			sig: None,
			repeat_vk: 0,
			repeat_steps: 0,
		};
		ed.highlight();
		ed.start_git();
		ed
	}

	pub fn title(&self) -> String {
		let name = self
			.path
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_else(|| self.path.to_string_lossy().into_owned());
		if self.modified {
			format!("{name} *")
		} else {
			name
		}
	}

	pub fn text(&self) -> String {
		let mut out = String::new();
		for (i, line) in self.lines.iter().enumerate() {
			if i > 0 {
				out.push('\n');
			}
			out.extend(line.iter());
		}
		out
	}

	pub fn ever_modified(&self) -> bool {
		self.modified || !self.undo.is_empty() || !self.redo.is_empty()
	}

	pub fn replace_all(&mut self, content: &str) {
		self.push_undo();
		self.lines = content.split('\n').map(|l| l.chars().collect()).collect();
		if self.lines.is_empty() {
			self.lines.push(Vec::new());
		}
		if self.lines.iter().all(|l| l.is_empty()) {
			self.lines.clear();
			self.lines.push(Vec::new());
		}
		self.cursor = (0, 0);
		self.anchor = None;
		self.modified = true;
		self.hl_dirty = true;
		self.auto_closed.clear();
		self.snippet = None;
		self.lsp_changed();
		self.clamp_cursor();
		self.highlight();
	}

	pub fn save(&mut self) -> std::io::Result<()> {
		let text = self.text();
		std::fs::write(&self.path, text)?;
		self.modified = false;
		self.status = String::new();
		self.refresh_git();
		Ok(())
	}

	/// The gutter's width in cells: one sign column, the line numbers, and
	/// half a cell of air before the text.
	pub fn gutter_cells(&self) -> usize {
		2 + self.line_digits()
	}

	fn line_digits(&self) -> usize {
		self.lines.len().max(1).to_string().len()
	}

	/// Watch the file's repo so the gutter can show its changes; files outside
	/// a repository get no signs.
	fn start_git(&mut self) {
		let Some((root, rel)) = git_repo(&self.path) else {
			return;
		};
		self.git = Some(GitWatch { root, rel, signs: Vec::new() });
		self.refresh_git();
	}

	fn refresh_git(&mut self) {
		let Some(watch) = &mut self.git else {
			return;
		};
		let signs = git_signs(&watch.root, &watch.rel, self.lines.len());
		watch.signs = signs;
	}

	fn git_touched(&mut self) {
		if self.git.is_some() {
			self.git_at = Some(Instant::now() + Duration::from_millis(250));
		}
	}

	fn clamp_cursor(&mut self) {
		if self.lines.is_empty() {
			self.lines.push(Vec::new());
		}
		self.cursor.0 = self.cursor.0.min(self.lines.len() - 1);
		self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
	}

	fn push_undo(&mut self) {
		self.undo.push((self.lines.clone(), self.cursor));
		if self.undo.len() > 300 {
			self.undo.remove(0);
		}
		self.redo.clear();
		self.git_touched();
	}

	pub fn undo(&mut self) {
		self.snippet = None;
		if let Some((lines, cursor)) = self.undo.pop() {
			self.redo
				.push((std::mem::replace(&mut self.lines, lines), self.cursor));
			self.cursor = cursor;
			self.anchor = None;
			self.column_goal = None;
			self.auto_closed.clear();
			self.modified = true;
			self.hl_dirty = true;
			self.lsp_changed();
			self.git_touched();
			self.highlight();
		}
	}

	pub fn redo(&mut self) {
		self.snippet = None;
		if let Some((lines, cursor)) = self.redo.pop() {
			self.undo
				.push((std::mem::replace(&mut self.lines, lines), self.cursor));
			self.cursor = cursor;
			self.anchor = None;
			self.column_goal = None;
			self.auto_closed.clear();
			self.modified = true;
			self.hl_dirty = true;
			self.lsp_changed();
			self.git_touched();
			self.highlight();
		}
	}

	pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
		let anchor = self.anchor?;
		if anchor == self.cursor {
			return None;
		}
		if anchor <= self.cursor {
			Some((anchor, self.cursor))
		} else {
			Some((self.cursor, anchor))
		}
	}

	pub fn has_selection(&self) -> bool {
		self.selection().is_some()
	}

	/// _validateAutoClosedActions：选区整体落在一对括号内部才算数（括号对必须在同一行），
	/// 离开这一对以后再回来也不会复活（VS Code 会直接删掉这条记录）。
	fn validate_auto_closed(&mut self) {
		let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
		self.auto_closed.retain(|a| {
			let encl_end = (a.close.0, a.close.1 + a.close_len);
			start.0 == end.0
				&& a.open.0 == a.close.0
				&& start.0 == a.open.0
				&& start > a.open
				&& end < encl_end
		});
	}

	fn auto_closed_positions(&self) -> Vec<(usize, usize)> {
		self.auto_closed.iter().map(|a| a.close).collect()
	}

	pub fn selected_text(&self) -> String {
		let Some((a, b)) = self.selection() else {
			return String::new();
		};
		let mut out = String::new();
		for row in a.0..=b.0 {
			let line = &self.lines[row];
			let from = if row == a.0 { a.1.min(line.len()) } else { 0 };
			let to = if row == b.0 {
				b.1.min(line.len())
			} else {
				line.len()
			};
			out.extend(&line[from..to]);
			if row != b.0 {
				out.push('\n');
			}
		}
		out
	}

	pub fn select_all(&mut self) {
		self.auto_closed.clear();
		self.anchor = Some((0, 0));
		let last = self.lines.len() - 1;
		self.cursor = (last, self.lines[last].len());
	}

	pub fn copy(&self) -> String {
		self.selected_text()
	}

	pub fn cut(&mut self) -> String {
		if self.read_only {
			return String::new();
		}
		let text = self.selected_text();
		if !text.is_empty() {
			self.delete_selection();
		}
		text
	}

	pub fn paste(&mut self, text: &str) {
		if self.read_only || text.is_empty() {
			return;
		}
		self.push_undo();
		self.delete_selection_inner();
		for (i, part) in text.replace("\r\n", "\n").split('\n').enumerate() {
			if i > 0 {
				self.split_line();
			}
			for c in part.chars() {
				if c != '\r' {
					self.insert_char_raw(c);
				}
			}
		}
		self.modified = true;
		self.hl_dirty = true;
		self.validate_auto_closed();
	}

	pub fn paste_clipboard(&mut self) {
		match clipboard::paste() {
			Some(clipboard::Paste::Text(text)) => self.paste(&text),
			Some(clipboard::Paste::Image(_)) => {
				self.status = String::from("solid 只编辑文本；图片请在 even 或 AI 页面粘贴。");
			}
			None => {}
		}
	}

	fn delete_selection_inner(&mut self) -> bool {
		let Some((a, b)) = self.selection() else {
			return false;
		};
		let (mut ar, mut ac) = a;
		let (br, mut bc) = b;
		bc = bc.min(self.lines[br].len());
		self.remap_auto_closed((ar, ac), (br, bc), 0, ac);
		if ar == br {
			self.lines[ar].drain(ac..bc);
		} else {
			let tail: Vec<char> = self.lines[br][bc..].to_vec();
			self.lines[ar].truncate(ac);
			self.lines[ar].extend(tail);
			self.lines.drain(ar + 1..=br);
		}
		self.cursor = (ar, ac);
		self.anchor = None;
		self.modified = true;
		self.hl_dirty = true;
		self.lsp_changed();
		true
	}

	pub fn delete_selection(&mut self) {
		if self.read_only {
			return;
		}
		self.push_undo();
		if !self.delete_selection_inner() {
			self.undo.pop();
		}
		self.validate_auto_closed();
	}

	fn insert_char_raw(&mut self, c: char) {
		let (row, col) = self.cursor;
		for a in &mut self.auto_closed {
			for p in [&mut a.open, &mut a.close] {
				if p.0 == row && p.1 >= col {
					p.1 += 1;
				}
			}
		}
		self.lines[row].insert(col, c);
		self.cursor.1 += 1;
		self.lsp_changed();
	}

	pub fn insert(&mut self, c: char, replace: bool) {
		if self.read_only {
			return;
		}
		self.push_undo();
		if self.delete_selection_inner() {
			// selection already removed
		}
		let (row, col) = self.cursor;
		if replace && col < self.lines[row].len() {
			self.lines[row][col] = c;
			self.cursor.1 += 1;
			self.lsp_changed();
		} else {
			self.insert_char_raw(c);
		}
		self.modified = true;
		self.hl_dirty = true;
		self.validate_auto_closed();
	}

	pub fn split_line(&mut self) {
		if self.read_only {
			return;
		}
		self.push_undo();
		self.delete_selection_inner();
		let (row, col) = self.cursor;
		self.remap_auto_closed((row, col), (row, col), 1, 0);
		let tail: Vec<char> = self.lines[row][col..].to_vec();
		self.lines[row].truncate(col);
		self.lines.insert(row + 1, tail);
		self.cursor = (row + 1, 0);
		self.modified = true;
		self.hl_dirty = true;
		self.lsp_changed();
		self.validate_auto_closed();
	}

	pub fn backspace(&mut self) {
		if self.read_only {
			return;
		}
		if self.has_selection() {
			self.delete_selection();
			return;
		}
		let (row, col) = self.cursor;
		if col > 0 {
			let line_text: String = self.lines[row].iter().collect();
			if let Some(config) = langconf::configuration(self.lang) {
				if langconf::is_auto_closing_pair_delete(
					config,
					&line_text,
					col + 1,
					&self.auto_closed_positions(),
					row + 1,
				) {
					self.push_undo();
					self.remap_auto_closed((row, col - 1), (row, col + 1), 0, col - 1);
					self.lines[row].remove(col);
					self.lines[row].remove(col - 1);
					self.cursor.1 -= 1;
					self.modified = true;
					self.hl_dirty = true;
					self.lsp_changed();
					self.validate_auto_closed();
					return;
				}
			}
			self.push_undo();
			self.remap_auto_closed((row, col - 1), (row, col), 0, col - 1);
			self.lines[row].remove(col - 1);
			self.cursor.1 -= 1;
			self.modified = true;
			self.hl_dirty = true;
			self.lsp_changed();
		} else if row > 0 {
			self.push_undo();
			let prev_len = self.lines[row - 1].len();
			self.remap_auto_closed((row - 1, prev_len), (row, 0), 0, prev_len);
			let line = self.lines.remove(row);
			self.lines[row - 1].extend(line);
			self.cursor = (row - 1, prev_len);
			self.modified = true;
			self.hl_dirty = true;
			self.lsp_changed();
		}
		self.validate_auto_closed();
	}

	pub fn delete(&mut self) {
		if self.read_only {
			return;
		}
		if self.has_selection() {
			self.delete_selection();
			return;
		}
		let (row, col) = self.cursor;
		if col < self.lines[row].len() {
			self.push_undo();
			self.remap_auto_closed((row, col), (row, col + 1), 0, col);
			self.lines[row].remove(col);
			self.modified = true;
			self.hl_dirty = true;
			self.lsp_changed();
		} else if row + 1 < self.lines.len() {
			self.push_undo();
			let len = self.lines[row].len();
			self.remap_auto_closed((row, len), (row + 1, 0), 0, len);
			let next = self.lines.remove(row + 1);
			self.lines[row].extend(next);
			self.modified = true;
			self.hl_dirty = true;
			self.lsp_changed();
		}
		self.validate_auto_closed();
	}

	/// 系统把按住不放报成同一个键的连续按下；每多一次多走一格，三格封顶，
	/// 松开或换键就回到一格。
	fn repeat_step(&mut self, vk: u16) -> i32 {
		if self.repeat_vk == vk {
			self.repeat_steps = (self.repeat_steps + 1).min(2);
		} else {
			self.repeat_vk = vk;
			self.repeat_steps = 0;
		}
		(self.repeat_steps + 1) as i32
	}

	pub fn move_cursor(&mut self, dr: i32, dc: i32, extend: bool) {
		self.snippet = None;
		if extend {
			if self.anchor.is_none() {
				self.anchor = Some(self.cursor);
			}
		} else {
			self.anchor = None;
		}
		let (mut row, mut col) = self.cursor;
		if dr != 0 {
			let goal = *self.column_goal.get_or_insert(col);
			if dr < 0 {
				row = row.saturating_sub((-dr) as usize);
			} else {
				row = (row + dr as usize).min(self.lines.len() - 1);
			}
			col = goal.min(self.lines[row].len());
		}
		if dc != 0 {
			self.column_goal = None;
			if dc < 0 {
				if col >= (-dc) as usize {
					col -= (-dc) as usize;
				} else if row > 0 {
					row -= 1;
					col = self.lines[row].len();
				} else {
					col = 0;
				}
			} else {
				col = (col + dc as usize).min(self.lines[row].len());
			}
		}
		self.cursor = (row, col);
		self.validate_auto_closed();
		self.sig_moved();
	}

	pub fn home(&mut self, extend: bool) {
		if extend && self.anchor.is_none() {
			self.anchor = Some(self.cursor);
		} else if !extend {
			self.anchor = None;
		}
		self.cursor.1 = 0;
		self.column_goal = None;
		self.validate_auto_closed();
		self.sig_moved();
	}

	pub fn end(&mut self, extend: bool) {
		if extend && self.anchor.is_none() {
			self.anchor = Some(self.cursor);
		} else if !extend {
			self.anchor = None;
		}
		self.cursor.1 = self.lines[self.cursor.0].len();
		self.column_goal = None;
		self.validate_auto_closed();
		self.sig_moved();
	}

	pub fn word_left(&mut self, extend: bool) {
		if extend && self.anchor.is_none() {
			self.anchor = Some(self.cursor);
		} else if !extend {
			self.anchor = None;
		}
		let (mut r, mut c) = self.cursor;
		let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
		while r > 0 || c > 0 {
			if c == 0 {
				r -= 1;
				c = self.lines[r].len();
				continue;
			}
			c -= 1;
			if c < self.lines[r].len() && is_word(self.lines[r][c]) {
				while c > 0 && is_word(self.lines[r][c - 1]) {
					c -= 1;
				}
				break;
			}
		}
		self.cursor = (r, c);
		self.column_goal = None;
		self.validate_auto_closed();
	}

	pub fn word_right(&mut self, extend: bool) {
		if extend && self.anchor.is_none() {
			self.anchor = Some(self.cursor);
		} else if !extend {
			self.anchor = None;
		}
		let (mut r, mut c) = self.cursor;
		let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
		loop {
			if c >= self.lines[r].len() {
				if r + 1 >= self.lines.len() {
					break;
				}
				r += 1;
				c = 0;
				continue;
			}
			if is_word(self.lines[r][c]) {
				while c < self.lines[r].len() && is_word(self.lines[r][c]) {
					c += 1;
				}
				break;
			}
			c += 1;
		}
		self.cursor = (r, c);
		self.column_goal = None;
		self.validate_auto_closed();
	}

	pub fn set_cursor(&mut self, row: usize, col: usize, extend: bool) {
		if extend {
			if self.anchor.is_none() {
				self.anchor = Some(self.cursor);
			}
		} else {
			self.anchor = None;
		}
		self.cursor = (row.min(self.lines.len() - 1), col);
		self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
		self.column_goal = None;
		self.validate_auto_closed();
	}

	pub fn select_word(&mut self, row: usize, col: usize) {
		let row = row.min(self.lines.len() - 1);
		let line = &self.lines[row];
		let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
		let mut start = col.min(line.len());
		let mut end = start;
		if start < line.len() && is_word(line[start]) {
			while start > 0 && is_word(line[start - 1]) {
				start -= 1;
			}
			while end < line.len() && is_word(line[end]) {
				end += 1;
			}
		} else if start < line.len() {
			end = start + 1;
		}
		self.anchor = Some((row, start));
		self.cursor = (row, end.saturating_sub(1).max(start));
		self.validate_auto_closed();
	}

	pub fn header(&self) -> String {
		format!("{} ({})", self.title(), self.lang.name())
	}

	/// VS Code 的 EnterOperation（autoIndent 固定 Full）。按选区 Range 计算，不先删选区。
	pub fn enter(&mut self) {
		if self.read_only {
			return;
		}
		self.push_undo();
		self.highlight();
		let empty = langconf::LanguageConfiguration::default();
		let config = langconf::configuration(self.lang).unwrap_or(&empty);
		let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
		let edit = langconf::get_enter_edit(
			&*self,
			config,
			start.0 + 1,
			start.1 + 1,
			end.0 + 1,
			end.1 + 1,
		);
		let from_row = (edit.from_line - 1).min(self.lines.len() - 1);
		let from_col = (edit.from_column - 1).min(self.lines[from_row].len());
		let to_row = (edit.to_line - 1).min(self.lines.len() - 1);
		let to_col = (edit.to_column - 1).min(self.lines[to_row].len());
		let (from_row, to_row, from_col, to_col) = if from_row < to_row {
			(from_row, to_row, from_col, to_col)
		} else {
			(from_row, from_row, from_col, from_col.max(to_col))
		};
		let prefix: Vec<char> = self.lines[from_row][..from_col].to_vec();
		let suffix: Vec<char> = self.lines[to_row][to_col..].to_vec();
		let mut parts: Vec<Vec<char>> =
			edit.text.split('\n').map(|part| part.chars().collect()).collect();
		parts[0].splice(0..0, prefix);
		let last = parts.len() - 1;
		let last_len = parts[last].len();
		parts[last].extend(suffix);
		self.lines.splice(from_row..=to_row, parts);
		self.remap_auto_closed((from_row, from_col), (to_row, to_col), last, last_len);
		let new_row = (from_row as isize + last as isize + edit.line_delta).max(0) as usize;
		let new_row = new_row.min(self.lines.len() - 1);
		let new_col = (last_len as isize + edit.column_delta).max(0) as usize;
		let new_col = new_col.min(self.lines[new_row].len());
		self.cursor = (new_row, new_col);
		self.anchor = None;
		self.column_goal = None;
		self.modified = true;
		self.hl_dirty = true;
		self.validate_auto_closed();
		self.lsp_changed();
	}

	/// 替换 `[from, to)` 这段文本后，自动闭合记录跟着走：
	/// 落在被替换区间里的作废，之后的记录按行差平移，end 行上的列改到后缀起点。
	/// `suffix_column` 是被替换文本末尾之后那段后缀在新行里的起始列。
	fn remap_auto_closed(
		&mut self,
		from: (usize, usize),
		to: (usize, usize),
		inserted_lines: usize,
		suffix_column: usize,
	) {
		let row_delta = inserted_lines as isize - (to.0 as isize - from.0 as isize);
		let map = |p: (usize, usize)| -> Option<(usize, usize)> {
			if p < from {
				return Some(p);
			}
			if p < to {
				return None;
			}
			let row = (p.0 as isize + row_delta) as usize;
			let col = if p.0 == to.0 {
				p.1 - to.1 + suffix_column
			} else {
				p.1
			};
			Some((row, col))
		};
		self.auto_closed.retain_mut(|a| match (map(a.open), map(a.close)) {
			(Some(open), Some(close)) => {
				a.open = open;
				a.close = close;
				true
			}
			_ => false,
		});
	}

	/// cursorTypeOperations.ts 的输入管线：自动缩进 → 越过闭合符 → 自动闭合 → 包选 → 普通输入。
	fn type_char(&mut self, ch: char) {
		if self.read_only {
			return;
		}
		self.highlight();
		let mut ch_text = String::new();
		ch_text.push(ch);
		let Some(config) = langconf::configuration(self.lang) else {
			self.insert(ch, false);
			return;
		};
		if self.has_selection() {
			let lines: Vec<String> = self.lines.iter().map(|line| line.iter().collect()).collect();
			let Some((a, b)) = self.selection() else {
				return;
			};
			if langconf::is_surround_selection_type(
				config,
				&lines,
				a.0 + 1,
				a.1 + 1,
				b.0 + 1,
				b.1 + 1,
				&ch_text,
			) {
				let close = config
					.surrounding
					.iter()
					.find(|(open, _)| open == &ch_text)
					.map(|(_, close)| close.clone())
					.unwrap_or_default();
				self.surround(&ch_text, &close);
				return;
			}
			self.push_undo();
			self.delete_selection_inner();
			self.highlight();
			self.insert_char_raw(ch);
			self.column_goal = None;
			self.modified = true;
			self.hl_dirty = true;
			self.validate_auto_closed();
			return;
		}
		let (row, col) = self.cursor;
		let line_text: String = self.lines[row].iter().collect();
		let column = col + 1;

		if let Some(indentation) =
			langconf::get_indent_action_for_type(&*self, config, row + 1, column, &ch_text)
		{
			let current = langconf::leading_whitespace(&line_text);
			if indentation != langconf::normalize_indent(current) {
				let pair =
					langconf::get_auto_closing_pair(&*self, config, row + 1, column, &ch_text);
				self.apply_auto_indent_type(row, col, &indentation, ch, pair);
				return;
			}
		}

		if langconf::is_auto_closing_overtype(
			config,
			&line_text,
			column,
			&ch_text,
			&self.auto_closed_positions(),
			row + 1,
		) {
			// AutoClosingOvertypeOperation：用同样的字符替换这一位，光标越过。
			self.push_undo();
			self.cursor.1 += 1;
			self.column_goal = None;
			self.validate_auto_closed();
			return;
		}

		if let Some((_open, close)) =
			langconf::get_auto_closing_pair(&*self, config, row + 1, column, &ch_text)
		{
			self.push_undo();
			self.insert_char_raw(ch);
			let open_end = self.cursor;
			for c in close.chars() {
				self.insert_char_raw(c);
			}
			self.auto_closed.push(AutoClosed {
				open: (row, col),
				close: open_end,
				close_len: close.chars().count(),
			});
			self.cursor = open_end;
			self.column_goal = None;
			self.modified = true;
			self.hl_dirty = true;
			self.validate_auto_closed();
			return;
		}

		self.push_undo();
		self.insert_char_raw(ch);
		self.column_goal = None;
		self.modified = true;
		self.hl_dirty = true;
		self.validate_auto_closed();
	}

	fn apply_auto_indent_type(
		&mut self,
		row: usize,
		col: usize,
		indentation: &str,
		ch: char,
		pair: Option<(String, String)>,
	) {
		self.push_undo();
		let first_non_ws = self.lines[row].iter().position(|c| !c.is_whitespace());
		let mut text = langconf::normalize_indent(indentation);
		if let Some(first) = first_non_ws {
			if first < col {
				text.extend(self.lines[row][first..col].iter().copied());
			}
		}
		let open_len = pair
			.as_ref()
			.map(|(open, _)| open.chars().count())
			.unwrap_or(0);
		if pair.is_none() || open_len > 1 {
			text.push(ch);
		}
		let new_len = text.chars().count();
		self.lines[row].splice(0..col, text.chars());
		self.shift_auto_closed_splice(row, col, new_len);
		self.cursor = (row, new_len);
		if let Some((open, close)) = pair {
			if open_len > 1 {
				// 多字符开口（如 """）：缩进编辑里已经带上 ch，只补闭合符。
				let open_start = (row, new_len.saturating_sub(open_len));
				let close_start = self.cursor;
				for c in close.chars() {
					self.insert_char_raw(c);
				}
				self.auto_closed.push(AutoClosed {
					open: open_start,
					close: close_start,
					close_len: close.chars().count(),
				});
				self.cursor = close_start;
			} else {
				let open_start = self.cursor;
				for c in open.chars() {
					self.insert_char_raw(c);
				}
				let open_end = self.cursor;
				for c in close.chars() {
					self.insert_char_raw(c);
				}
				self.auto_closed.push(AutoClosed {
					open: open_start,
					close: open_end,
					close_len: close.chars().count(),
				});
				self.cursor = open_end;
			}
		}
		self.column_goal = None;
		self.modified = true;
		self.hl_dirty = true;
		self.lsp_changed();
		self.validate_auto_closed();
	}

	fn shift_auto_closed_splice(&mut self, row: usize, to: usize, new_len: usize) {
		self.auto_closed.retain(|a| {
			!(a.open.0 == row && a.open.1 < to) && !(a.close.0 == row && a.close.1 < to)
		});
		for a in &mut self.auto_closed {
			for p in [&mut a.open, &mut a.close] {
				if p.0 == row {
					p.1 = p.1 - to + new_len;
				}
			}
		}
	}

	/// SurroundSelectionCommand：在选区两端插入字符，选区仍罩住原内容。
	fn surround(&mut self, open: &str, close: &str) {
		let Some((a, b)) = self.selection() else {
			return;
		};
		self.push_undo();
		let close_chars: Vec<char> = close.chars().collect();
		let open_chars: Vec<char> = open.chars().collect();
		let close_at = b.1.min(self.lines[b.0].len());
		self.remap_auto_closed((b.0, close_at), (b.0, close_at), 0, close_at + close_chars.len());
		self.lines[b.0].splice(close_at..close_at, close_chars.iter().copied());
		let open_at = a.1.min(self.lines[a.0].len());
		self.remap_auto_closed((a.0, open_at), (a.0, open_at), 0, open_at + open_chars.len());
		self.lines[a.0].splice(open_at..open_at, open_chars.iter().copied());
		let end = if a.0 == b.0 {
			(b.0, b.1 + open_chars.len())
		} else {
			(b.0, b.1)
		};
		self.anchor = Some((a.0, a.1 + open_chars.len()));
		self.cursor = end;
		self.column_goal = None;
		self.modified = true;
		self.hl_dirty = true;
		self.lsp_changed();
		self.validate_auto_closed();
	}

	fn processed_row(
		&self,
		row: usize,
		skip: usize,
		take: usize,
		config: &langconf::LanguageConfiguration,
	) -> String {
		let Some(chars) = self.lines.get(row) else {
			return String::new();
		};
		let text: Vec<char> = chars.iter().skip(skip).take(take).copied().collect();
		let kinds: Vec<ScopeKind> = self
			.scopes
			.get(row)
			.map(|kinds| kinds.iter().skip(skip).take(take).copied().collect())
			.unwrap_or_default();
		langconf::process_line(&text, &kinds, config)
	}

	/// bracketPairs.hasUnmatchedClosingBracketAfter（安装版 workbench.desktop.main.js 的
	/// bracketPairsTree 那一份）：括号树里记的是 missingOpeningBracketIds，全文都没有开括号
	/// 可配的闭合符才算“未配对”，再看它是否落在插入点之后。光标前已经配上的闭合符不算。
	fn scan_unmatched_close(&self, line: usize, column: usize, open: &str) -> bool {
		let Some(config) = langconf::configuration(self.lang) else {
			return false;
		};
		let Some(pair) = config.auto_closing.iter().find(|pair| pair.open == open) else {
			return false;
		};
		let open_chars: Vec<char> = pair.open.chars().collect();
		let close_chars: Vec<char> = pair.close.chars().collect();
		if open_chars.is_empty() || close_chars.is_empty() {
			return false;
		}
		let mut depth = 0usize;
		for (row, chars) in self.lines.iter().enumerate() {
			let mut i = 0;
			while i < chars.len() {
				let scoped = self
					.scopes
					.get(row)
					.and_then(|scopes| scopes.get(i))
					.is_some_and(|kind| *kind != ScopeKind::Other);
				if scoped {
					i += 1;
					continue;
				}
				if starts_with(chars, i, &open_chars) {
					depth += 1;
					i += open_chars.len();
					continue;
				}
				if starts_with(chars, i, &close_chars) {
					if depth == 0 {
						if row + 1 > line || (row + 1 == line && i + 1 >= column) {
							return true;
						}
					} else {
						depth -= 1;
					}
					i += close_chars.len();
					continue;
				}
				i += 1;
			}
		}
		false
	}

	/// Start the language server for this buffer, once per (language, root).
	/// Called by the app after opening; unit tests never start servers.
	pub fn attach_lsp(&mut self) {
		if self.lsp.is_some() || self.read_only {
			return;
		}
		let text = self.text();
		self.lsp = lsp::Doc::open(&self.path, self.lang.name(), &text);
	}

	fn lsp_changed(&mut self) {
		if self.lsp.is_some() {
			self.lsp_edit_at = Some(Instant::now());
		}
	}

	/// Insert-mode autotrigger: server trigger characters start completion
	/// (`completion.lua` `on_insert_char_pre`); every other typed character
	/// arms VS Code's `_doTriggerQuickSuggest` timer.
	fn lsp_after_type(&mut self, ch: char) {
		let Some(doc) = &self.lsp else {
			return;
		};
		// Blink's `on_insert_char_pre`: a trigger character opens the
		// signature window; any other key while it is open re-requests so the
		// active parameter follows the caret.
		let sig_trigger = doc
			.signature_trigger_chars()
			.iter()
			.any(|t| t == &ch.to_string());
		if sig_trigger {
			self.request_signature(Some(ch.to_string()));
		} else if self.sig.is_some() {
			self.request_signature(None);
		}
		let Some(doc) = &self.lsp else {
			return;
		};
		if self.completion.is_some() {
			// Neovim keeps the pum and only re-requests when the last result
			// said isIncomplete.
			let incomplete = self.completion.as_ref().is_some_and(|p| p.incomplete);
			if incomplete {
				self.request_completion(3, None);
			}
			return;
		}
		if doc.trigger_chars().iter().any(|t| t == &ch.to_string()) {
			self.request_completion(2, Some(ch.to_string()));
			return;
		}
		self.suggest_at = Some(Instant::now() + Duration::from_millis(QUICK_SUGGEST_DELAY_MS));
		self.suggest_pos = self.cursor;
	}

	/// VS Code `WordBasedSuggestionsController.shouldAutoTrigger`: a word that
	/// is not a number must end at the cursor.
	fn quick_suggest_word(&self) -> bool {
		let (row, col) = self.cursor;
		let (word_row, start) = self.word_start();
		if word_row != row || start >= col {
			return false;
		}
		let word: String = self.lines[row][start..col].iter().collect();
		!is_js_number(&word)
	}

	/// `_doTriggerQuickSuggest`'s token check: `quickSuggestions` defaults to
	/// `other: on, comments: off, strings: off`.
	fn quick_suggest_ready(&self) -> bool {
		if self.completion.is_some() || self.lsp.is_none() {
			return false;
		}
		if self.cursor != self.suggest_pos {
			return false;
		}
		let (row, col) = self.cursor;
		if langconf::Model::scope_before(self, row + 1, col + 1) != ScopeKind::Other {
			return false;
		}
		self.quick_suggest_word()
	}

	/// Word start on the current line: `\k*$`, keyword = alphanumeric or `_`.
	fn word_start(&self) -> (usize, usize) {
		let (row, col) = self.cursor;
		let line = &self.lines[row];
		let mut start = col;
		while start > 0 {
			let c = line[start - 1];
			if c.is_alphanumeric() || c == '_' {
				start -= 1;
			} else {
				break;
			}
		}
		(row, start)
	}

	/// Character offset of a (row, column) in the document, a line break
	/// counting as one character — the numbering snippet stops use.
	fn doc_offset(&self, (row, col): (usize, usize)) -> usize {
		let mut offset = 0;
		for (i, line) in self.lines.iter().enumerate() {
			if i == row {
				return offset + col.min(line.len());
			}
			offset += line.len() + 1;
		}
		offset
	}

	fn doc_pos(&self, offset: usize) -> (usize, usize) {
		let mut left = offset;
		for (row, line) in self.lines.iter().enumerate() {
			if left <= line.len() {
				return (row, left);
			}
			left -= line.len() + 1;
		}
		let last = self.lines.len() - 1;
		(last, self.lines[last].len())
	}

	/// Shift the snippet stops across one keystroke's edit, found by diffing
	/// the document text around the key. `prefix` is the unchanged head,
	/// `removed` what the key deleted, `inserted` what it typed.
	fn snippet_edited(&mut self, prefix: usize, removed: usize, inserted: usize) {
		let Some(snippet) = &mut self.snippet else {
			return;
		};
		let end = prefix + removed;
		for stop in &mut snippet.stops {
			let (s, e) = *stop;
			let map = |p: usize| {
				if p <= prefix {
					p
				} else if p >= end {
					p - removed + inserted
				} else {
					prefix
				}
			};
			let mut ns = map(s);
			let mut ne = map(e);
			// A replacement that swallowed the stop hands it the new text;
			// typing inside or at the edge of a stop extends it.
			if removed > 0 && s >= prefix && e <= end {
				ne = prefix + inserted;
			} else if removed == 0 && s <= prefix && prefix <= e {
				ne = ne.max(e + inserted);
			}
			if ne < ns {
				ne = ns;
			}
			*stop = (ns, ne);
		}
	}

	fn snippet_select(&mut self, index: usize) {
		let Some(snippet) = &mut self.snippet else {
			return;
		};
		let Some(&(start, end)) = snippet.stops.get(index) else {
			return;
		};
		snippet.index = index;
		let a = self.doc_pos(start);
		let b = self.doc_pos(end);
		self.cursor = b;
		self.anchor = (a != b).then_some(a);
		self.column_goal = None;
		self.clamp_cursor();
	}

	/// Tab / Shift+Tab inside a snippet. Returns false at the last (or first)
	/// stop, where VS Code ends the session and lets the key act normally.
	fn snippet_step(&mut self, backward: bool) -> bool {
		let Some(snippet) = &self.snippet else {
			return false;
		};
		let next = if backward {
			snippet.index.checked_sub(1)
		} else if snippet.index + 1 < snippet.stops.len() {
			Some(snippet.index + 1)
		} else {
			None
		};
		match next {
			Some(index) => {
				self.snippet_select(index);
				true
			}
			None => {
				self.snippet = None;
				self.anchor = None;
				false
			}
		}
	}

	fn request_completion(&mut self, kind: u8, trigger: Option<String>) {
		if self.lsp.is_none() {
			return;
		}
		// The server must see the typed text before it answers for the cursor
		// position; Neovim sends didChange synchronously for the same reason.
		if self.lsp_edit_at.take().is_some() {
			let text = self.text();
			if let Some(doc) = &mut self.lsp {
				doc.sync(&text);
			}
		}
		let (row, col) = self.cursor;
		let character = lsp::utf16_col(&self.lines[row], col);
		let start = self.word_start();
		let req = self
			.lsp
			.as_ref()
			.and_then(|doc| doc.complete(row, character, trigger, kind));
		let Some(req) = req else {
			return;
		};
		// An `isIncomplete` re-request keeps the pum and its items on screen
		// until the new list arrives; clearing them would make Enter fall
		// through to a newline while the user is still choosing
		// (`completion.lua` `on_insert_char_pre`).
		if kind == 3 {
			if let Some(popup) = &mut self.completion {
				popup.req = req;
				popup.done = false;
				popup.incomplete = false;
				return;
			}
		}
		self.completion = Some(Completion {
			req,
			items: Vec::new(),
			index: 0,
			incomplete: false,
			done: false,
			start,
		});
	}

	/// Ask the server for the call the cursor is inside. Blink re-requests on
	/// every cursor move while the window is open, so the active parameter
	/// keeps up with the caret.
	fn request_signature(&mut self, trigger: Option<String>) {
		if self.lsp.is_none() {
			return;
		}
		if self.lsp_edit_at.take().is_some() {
			let text = self.text();
			if let Some(doc) = &mut self.lsp {
				doc.sync(&text);
			}
		}
		let (row, col) = self.cursor;
		let character = lsp::utf16_col(&self.lines[row], col);
		let kind = if trigger.is_some() { 2 } else { 1 };
		let req = self
			.lsp
			.as_ref()
			.and_then(|doc| doc.signature(row, character, trigger, kind));
		self.sig = req.map(|req| Signature {
			req,
			done: false,
			help: None,
			range: None,
		});
	}

	/// Blink re-requests the open signature window every time the caret moves
	/// (`CursorMovedI`), not only after a typed character.
	fn sig_moved(&mut self) {
		if self.sig.is_some() {
			self.request_signature(None);
		}
	}

	fn completion_filtered(&self) -> Vec<usize> {
		let Some(popup) = &self.completion else {
			return Vec::new();
		};
		let prefix = if popup.start.0 == self.cursor.0 && popup.start.1 <= self.cursor.1 {
			self.lines[self.cursor.0][popup.start.1..self.cursor.1]
				.iter()
				.collect::<String>()
		} else {
			String::new()
		};
		popup
			.items
			.iter()
			.enumerate()
			.filter(|(_, item)| lsp::item_matches(item, &prefix))
			.map(|(i, _)| i)
			.collect()
	}

	/// Apply the selected item. Neovim's pum replaces the typed text from its
	/// start column with the completion word; the server's textEdit range is
	/// only used to widen that start, never to rewrite text the user typed
	/// after the request was made. additionalTextEdits come along in the same
	/// change set.
	fn accept_completion(&mut self, item_index: usize) {
		let Some(popup) = self.completion.take() else {
			return;
		};
		let Some(item) = popup.items.get(item_index).cloned() else {
			return;
		};
		self.snippet = None;
		let (row, col) = self.cursor;
		let line = &self.lines[row];
		let mut start_col = if popup.start.0 == row {
			popup.start.1.min(col)
		} else {
			col
		};
		if let Some(edit) = &item.text_edit {
			if edit.start.line as usize == row {
				start_col = start_col.min(lsp::char_col(line, edit.start.character));
			}
		}
		let text = lsp::completion_text(&item);
		let (plain, stops) = if item.insert_format == 2 {
			lsp::expand_snippet(&text)
		} else {
			(text, Vec::new())
		};
		let start_utf16 = lsp::utf16_col(line, start_col);
		// An additional edit above the main one (clangd prepending an
		// #include) shifts the text the main edit inserts.
		let mut base_row = row;
		let mut base_col = start_col;
		for edit in &item.additional {
			if edit.end.line as usize > row
				|| (edit.end.line as usize == row && edit.end.character > start_utf16)
			{
				continue;
			}
			let rows = edit.end.line.saturating_sub(edit.start.line) as i64;
			let added = edit.new_text.matches('\n').count() as i64 - rows;
			base_row = (base_row as i64 + added).max(0) as usize;
			if added == 0 && edit.start.line == edit.end.line && edit.end.line as usize == row {
				let last = edit.new_text.rsplit('\n').next().unwrap_or("");
				let cols = last.chars().count() as i64
					- (edit.end.character as i64 - edit.start.character as i64);
				base_col = (base_col as i64 + cols).max(0) as usize;
			}
		}
		let main = lsp::Edit {
			start: lsp::Pos { line: row as u32, character: start_utf16 },
			end: lsp::Pos { line: row as u32, character: lsp::utf16_col(line, col) },
			new_text: plain.clone(),
		};
		self.push_undo();
		let mut edits = item.additional.clone();
		edits.push(main);
		lsp::apply_edits(&mut self.lines, &edits);
		if base_row >= self.lines.len() {
			base_row = self.lines.len() - 1;
		}
		base_col = base_col.min(self.lines[base_row].len());
		let base = self.doc_offset((base_row, base_col));
		self.anchor = None;
		self.column_goal = None;
		self.cursor = self.doc_pos(base + plain.chars().count());
		self.clamp_cursor();
		if !stops.is_empty() {
			let mapped: Vec<(usize, usize)> =
				stops.iter().map(|s| (base + s.start, base + s.end)).collect();
			self.snippet_gen += 1;
			self.snippet = Some(Snippet { stops: mapped, index: 0, id: self.snippet_gen });
			self.snippet_select(0);
		}
		self.auto_closed.clear();
		self.modified = true;
		self.hl_dirty = true;
		self.lsp_changed();
		// Blink's `show_on_accept_on_trigger_character`: accepting a function
		// leaves a trigger character at the caret, which opens the signature.
		let (row, col) = self.cursor;
		let ch = self
			.lines
			.get(row)
			.and_then(|line| line.get(col))
			.copied();
		if let Some(ch) = ch {
			let trigger = self
				.lsp
				.as_ref()
				.is_some_and(|doc| doc.signature_trigger_chars().iter().any(|t| t == &ch.to_string()));
			if trigger {
				self.request_signature(Some(ch.to_string()));
			}
		}
	}

	/// Keep the server and diagnostics in step with the buffer. Runs every
	/// paint (the app ticks at 16ms), so the didChange debounce lives here.
	fn poll_lsp(&mut self) {
		if self.lsp.is_none() {
			return;
		}
		if let Some(at) = self.suggest_at {
			if Instant::now() >= at {
				self.suggest_at = None;
				if self.quick_suggest_ready() {
					self.request_completion(1, None);
				}
			}
		}
		if let Some(at) = self.lsp_edit_at {
			if at.elapsed() >= Duration::from_millis(150) {
				self.lsp_edit_at = None;
				let text = self.text();
				if let Some(doc) = &mut self.lsp {
					doc.sync(&text);
				}
			}
		}
		if let Some(popup) = &mut self.completion {
			if !popup.done {
				if let Some((items, incomplete)) =
					self.lsp.as_ref().and_then(|doc| doc.completion(popup.req))
				{
					popup.items = items;
					popup.incomplete = incomplete;
					popup.done = true;
					popup.index = 0;
					let row = popup.start.0;
					if let Some(line) = self.lines.get(row) {
						if let Some(col) = lsp::response_start(&popup.items, row, line) {
							popup.start = (row, col);
						}
					}
				}
			}
		}
		if self
			.completion
			.as_ref()
			.is_some_and(|popup| popup.done && self.completion_filtered().is_empty())
		{
			self.completion = None;
		}
		if let Some(req) = self.sig.as_ref().filter(|sig| !sig.done).map(|sig| sig.req) {
			if let Some((done, help)) =
				self.lsp.as_ref().and_then(|doc| doc.signature_help(req))
			{
				let range = help.as_ref().and_then(|help| {
					let triggers = self
						.lsp
						.as_ref()
						.map(|doc| doc.signature_trigger_chars())
						.unwrap_or_default();
					lsp::active_parameter_range(help, &triggers)
				});
				if let Some(sig) = &mut self.sig {
					sig.done = done;
					sig.help = help;
					sig.range = range;
				}
			}
		}
		if self
			.sig
			.as_ref()
			.is_some_and(|sig| sig.done && sig.help.is_none())
		{
			self.sig = None;
		}
		if let Some((generation, diags)) =
			self.lsp.as_ref().and_then(|doc| doc.diagnostics(self.diag_gen))
		{
			self.diag_gen = generation;
			self.diags = diags;
		}
	}

	fn diag_at_cursor(&self) -> Option<&lsp::Diag> {
		let (row, col) = self.cursor;
		let line = &self.lines[row];
		let pos = (row as u32, lsp::utf16_col(line, col));
		self.diags.iter().find(|d| {
			let start = (d.start.line, d.start.character);
			let end = (d.end.line, d.end.character);
			if start == end {
				start <= pos && pos <= end
			} else {
				start <= pos && pos < end
			}
		})
	}

	/// solid's own key map. Delta only hands over the key; what Ctrl+C means
	/// here is decided here.
	pub fn key(&mut self, key: &Key) -> bool {
		// The snippet stops follow the session through every edit; diffing the
		// text around the key keeps them right without teaching each editing
		// primitive about snippets.
		let before = self.snippet.as_ref().map(|s| (s.id, self.text()));
		let handled = self.key_inner(key);
		if let (Some((id, before)), Some(snippet)) = (before, self.snippet.as_ref()) {
			if snippet.id != id {
				return handled;
			}
			let after = self.text();
			let old: Vec<char> = before.chars().collect();
			let new: Vec<char> = after.chars().collect();
			let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
			let tail = old
				.len()
				.saturating_sub(prefix)
				.min(new.len().saturating_sub(prefix));
			let suffix = old
				.iter()
				.rev()
				.zip(new.iter().rev())
				.take(tail)
				.take_while(|(a, b)| a == b)
				.count();
			let removed = old.len() - prefix - suffix;
			let inserted = new.len() - prefix - suffix;
			if removed > 0 || inserted > 0 {
				self.snippet_edited(prefix, removed, inserted);
			}
		}
		handled
	}

	fn key_inner(&mut self, key: &Key) -> bool {
		if !key.down {
			if key.vk == self.repeat_vk {
				self.repeat_vk = 0;
				self.repeat_steps = 0;
			}
			return true;
		}
		// `cancelAndSet`: every keystroke drops a pending quick-suggest timer.
		self.suggest_at = None;
		let shift = key.shift;
		let ctrl = key.ctrl && !key.alt;
		if ctrl && key.vk == 0x20 {
			self.request_completion(1, None);
			return true;
		}
		if self.completion.is_some() {
			let filtered = self.completion_filtered();
			let shown = !filtered.is_empty();
			match key.vk {
				0x1B => {
					self.completion = None;
					self.sig = None;
					return true;
				}
				0x26 if shown => {
					if let Some(popup) = &mut self.completion {
						popup.index = popup.index.saturating_sub(1);
					}
					return true;
				}
				0x28 if shown => {
					if let Some(popup) = &mut self.completion {
						popup.index = (popup.index + 1).min(filtered.len() - 1);
					}
					return true;
				}
				0x0D | 0x09 if shown => {
					if let Some(&index) = filtered.get(self.completion.as_ref().unwrap().index) {
						self.accept_completion(index);
					}
					return true;
				}
				// The pum filters while the user keeps typing or deletes
				// (`completion.lua`); it only goes away on other keys.
				0x08 | 0x2E => {}
				_ => {
					let text_key = !key.ctrl && !key.alt || key.right_alt;
					let word_char = text_key
						&& key
							.ch
							.is_some_and(|ch| !ch.is_control() && (ch.is_alphanumeric() || ch == '_'));
					if !word_char {
						self.completion = None;
					}
				}
			}
		}
		if self.snippet.is_some() {
			match key.vk {
				0x1B => {
					self.snippet = None;
					return true;
				}
				0x09 if self.snippet_step(shift) => return true,
				0x21 | 0x22 | 0x23 | 0x24 | 0x25 | 0x26 | 0x27 | 0x28 => self.snippet = None,
				_ => {}
			}
		}
		match key.vk {
			0x41 if ctrl => self.select_all(),
			0x43 if ctrl => {
				if self.has_selection() {
					clipboard::set(&self.selected_text());
				}
			}
			0x58 if ctrl => {
				let text = self.cut();
				if !text.is_empty() {
					clipboard::set(&text);
				}
			}
			0x56 if ctrl => self.paste_clipboard(),
			0x5A if ctrl => {
				if shift {
					self.redo();
				} else {
					self.undo();
				}
			}
			0x59 if ctrl => self.redo(),
			0x53 if ctrl => {
				if let Err(err) = self.save() {
					self.status = format!("保存失败：{err}");
				}
			}
			0x0D => self.enter(),
			0x08 => self.backspace(),
			0x2E => self.delete(),
			0x09 => self.insert('\t', false),
			0x1B => self.sig = None,
			0x25 => {
				if ctrl {
					self.word_left(shift);
				} else {
					let step = self.repeat_step(0x25);
					self.move_cursor(0, -step, shift);
				}
			}
			0x27 => {
				if ctrl {
					self.word_right(shift);
				} else {
					let step = self.repeat_step(0x27);
					self.move_cursor(0, step, shift);
				}
			}
			0x26 => {
				let step = self.repeat_step(0x26);
				self.move_cursor(-step, 0, shift);
			}
			0x28 => {
				let step = self.repeat_step(0x28);
				self.move_cursor(step, 0, shift);
			}
			0x24 => self.home(shift),
			0x23 => self.end(shift),
			0x21 => self.move_cursor(-10, 0, shift),
			0x22 => self.move_cursor(10, 0, shift),
			_ => {
				if let Some(ch) = key.ch {
					// AltGr arrives as Ctrl+Alt; it is still text input.
					let text = !ch.is_control() && (!key.ctrl && !key.alt || key.right_alt);
					if text {
						self.type_char(ch);
						self.lsp_after_type(ch);
					}
				}
			}
		}
		true
	}

	/// A click at a screen cell inside the editor.
	pub fn click_screen(&mut self, screen_row: usize, screen_col: usize, extend: bool) {
		self.completion = None;
		self.snippet = None;
		self.repeat_vk = 0;
		self.repeat_steps = 0;
		let row = (self.scroll + screen_row).min(self.lines.len() - 1);
		// The gutter cells carry no text; a click on them lands on column 0.
		let cell = screen_col.saturating_sub(self.gutter_cells());
		let col = self.column_at(row, cell + self.hscroll);
		self.set_cursor(row, col, extend);
	}

	pub fn double_click_screen(&mut self, screen_row: usize, screen_col: usize) {
		self.completion = None;
		self.snippet = None;
		let row = (self.scroll + screen_row).min(self.lines.len() - 1);
		let cell = screen_col.saturating_sub(self.gutter_cells());
		let col = self.column_at(row, cell + self.hscroll);
		self.select_word(row, col);
	}

	pub fn wheel(&mut self, lines: i32) {
		self.completion = None;
		self.snippet = None;
		self.move_cursor(lines, 0, false);
	}

	fn column_at(&self, row: usize, target_cell: usize) -> usize {
		let Some(line) = self.lines.get(row) else {
			return 0;
		};
		let mut cell = 0usize;
		for (index, ch) in line.iter().enumerate() {
			if cell >= target_cell {
				return index;
			}
			cell += if *ch == '\t' {
				4 - cell % 4
			} else {
				char_cells(*ch)
			};
		}
		line.len()
	}

	pub fn highlight(&mut self) {
		if !self.hl_dirty {
			return;
		}
		self.hl_dirty = false;
		let lines = self.lines.len();
		self.hl = (0..lines)
			.map(|i| vec![Hl::Plain; self.lines[i].len()])
			.collect();
		self.scopes = (0..lines)
			.map(|i| vec![ScopeKind::Other; self.lines[i].len()])
			.collect();
		let Some(language) = self.lang.language() else {
			return;
		};
		let mut parser = Parser::new();
		if parser.set_language(&language).is_err() {
			return;
		}
		let src = self.text();
		let bytes = src.as_bytes();
		let Some(tree) = parser.parse(&src, None) else {
			return;
		};
		let mut starts = Vec::with_capacity(lines);
		let mut pos = 0usize;
		for line in &self.lines {
			starts.push(pos);
			pos += line.iter().map(|c| c.len_utf8()).sum::<usize>() + 1;
		}
		let linecol = |offset: usize| -> (usize, usize) {
			let idx = starts.partition_point(|&s| s <= offset).saturating_sub(1);
			let rel = offset.saturating_sub(starts.get(idx).copied().unwrap_or(0));
			let text = &self
				.lines
				.get(idx)
				.map(|l| l.iter().collect::<String>())
				.unwrap_or_default();
			let col = text.char_indices().take_while(|(b, _)| *b < rel).count();
			(idx, col)
		};
		let mut stack = vec![tree.root_node()];
		let lang = self.lang;
		while let Some(node) = stack.pop() {
			let class = classify(node, bytes, lang);
			let (sr, sc) = linecol(node.start_byte());
			let (er, ec) = linecol(node.end_byte());
			if class != Hl::Plain {
				for r in sr..=er.min(lines - 1) {
					let line_len = self.hl[r].len();
					let from = if r == sr { sc } else { 0 };
					let to = if r == er { ec } else { line_len };
					for c in from..to.min(line_len) {
						self.hl[r][c] = class;
					}
				}
			}
			if let Some(scope) = scope_of(node) {
				for r in sr..=er.min(lines - 1) {
					let line_len = self.scopes[r].len();
					let from = if r == sr { sc } else { 0 };
					let to = if r == er { ec } else { line_len };
					for c in from..to.min(line_len) {
						self.scopes[r][c] = scope;
					}
				}
			}
			let mut cursor = node.walk();
			for child in node.children(&mut cursor) {
				stack.push(child);
			}
		}
	}

	pub fn render(
		&mut self,
		canvas: &Canvas,
		area: Rect,
		focused: bool,
		show_cursor: bool,
		show_whitespace: bool,
	) {
		self.poll_lsp();
		if let Some(at) = self.git_at {
			if Instant::now() >= at {
				self.git_at = None;
				self.refresh_git();
			}
		}
		if !focused || !show_cursor {
			// Blink hides the signature when insert ends.
			self.sig = None;
		}
		self.highlight();
		let cw = canvas.cell_w;
		let ch = canvas.cell_h;
		let total_rows = (area.height() / ch).max(1) as usize;
		// A save error has nowhere else to show; the editor keeps it on its own
		// bottom row until the next successful save. A diagnostic under the
		// cursor uses the same row when there is no save error to show.
		let footer_text = if !self.status.is_empty() {
			Some(self.status.clone())
		} else {
			self.diag_at_cursor()
				.map(|d| format!("{}: {}", severity_name(d.severity), d.message))
		};
		let footer = if footer_text.is_some() { 1 } else { 0 };
		let rows = (total_rows - footer).max(1);
		let area_cols = (area.width() / cw).max(1) as usize;
		// The gutter stays put while the text scrolls: one sign column, the
		// absolute line numbers, and half a cell before the first character.
		let digits = self.line_digits();
		let gutter_px = (1 + digits) as i32 * cw + cw / 2;
		let text_left = area.left + gutter_px;
		let cols = (((area.width() - gutter_px).max(cw)) / cw) as usize;
		let bg = canvas::rgb(
			canvas::DEFAULT_BG.0,
			canvas::DEFAULT_BG.1,
			canvas::DEFAULT_BG.2,
		);
		canvas.fill(
			area.left,
			area.top,
			area.left + area_cols as i32 * cw,
			area.top + total_rows as i32 * ch,
			bg,
		);
		let saved = canvas.clip((
			area.left,
			area.top,
			area.left + area_cols as i32 * cw,
			area.top + total_rows as i32 * ch,
		));

		if self.cursor.0 < self.scroll {
			self.scroll = self.cursor.0;
		}
		if self.cursor.0 >= self.scroll + rows {
			self.scroll = self.cursor.0 + 1 - rows;
		}
		let sel = self.selection();
		let mut visual_col = 0usize;
		for (i, c) in self.lines[self.cursor.0]
			.iter()
			.take(self.cursor.1)
			.enumerate()
		{
			let _ = i;
			visual_col += if *c == '\t' {
				4 - visual_col % 4
			} else {
				char_cells(*c)
			};
		}
		if visual_col < self.hscroll {
			self.hscroll = visual_col;
		}
		if visual_col >= self.hscroll + cols {
			self.hscroll = visual_col + 1 - cols;
		}

		// Catppuccin Mocha's Normal foreground; text no token claimed uses it.
		let fg_plain = canvas::rgb(205, 214, 244);
		let number_plain = canvas::rgb(69, 71, 90);
		let number_current = canvas::rgb(249, 226, 175);
		let guide = canvas::rgb(49, 50, 68);
		let whitespace = canvas::rgb(78, 82, 106);
		for row in 0..rows {
			let line_idx = self.scroll + row;
			if line_idx >= self.lines.len() {
				break;
			}
			let y = area.top + row as i32 * ch;
			let line = &self.lines[line_idx];

			// Absolute line number, right-aligned, and the git sign.
			let number = (line_idx + 1).to_string();
			let nx = area.left + (1 + digits.saturating_sub(number.len())) as i32 * cw;
			let ncolor = if line_idx == self.cursor.0 {
				number_current
			} else {
				number_plain
			};
			canvas.text(nx, y, &number, ncolor);
			if let Some(sign) = self
				.git
				.as_ref()
				.and_then(|g| g.signs.get(line_idx).copied())
				.flatten()
			{
				let x = area.left + cw / 2 - 1;
				canvas.fill(x, y, x + 2, y + ch, sign_color(sign));
			}

			// Indent guides at the tab stops the line's indentation crosses.
			let mut indent_cells = 0usize;
			for c in line {
				match c {
					' ' => indent_cells += 1,
					'\t' => indent_cells += 4 - indent_cells % 4,
					_ => break,
				}
			}
			let mut level = 4usize;
			while level < indent_cells {
				if level >= self.hscroll {
					let screen_col = level - self.hscroll;
					if screen_col < cols {
						let x = text_left + screen_col as i32 * cw;
						canvas.fill(x, y, x + 1, y + ch, guide);
					}
				}
				level += 4;
			}

			let mut x_cells = 0usize;
			for (ci, c) in line.iter().enumerate() {
				let is_cursor = focused && show_cursor && self.cursor == (line_idx, ci);
				if *c == '\t' {
					let next = 4 - x_cells % 4;
					if show_whitespace && !is_cursor && x_cells >= self.hscroll {
						let screen_col = x_cells - self.hscroll;
						if screen_col < cols {
							// A tab runs to the next stop and ends in an arrowhead.
							let x0 = text_left + screen_col as i32 * cw + 1;
							let x1 = (x0 + next as i32 * cw).min(text_left + cols as i32 * cw);
							let mid = y + ch / 2;
							if x1 > x0 + 4 {
								canvas.line(x0, mid, x1 - 1, mid, whitespace);
								canvas.line(x1 - 4, mid - 3, x1 - 1, mid, whitespace);
								canvas.line(x1 - 4, mid + 3, x1 - 1, mid, whitespace);
							}
						}
					}
					x_cells += next;
					continue;
				}
				if x_cells >= self.hscroll {
					let screen_col = x_cells - self.hscroll;
					if screen_col >= cols {
						break;
					}
					let width = char_cells(*c);
					let x = text_left + screen_col as i32 * cw;
					let class = self
						.hl
						.get(line_idx)
						.and_then(|l| l.get(ci))
						.copied()
						.unwrap_or(Hl::Plain);
					let mut fg = hl_color(class, fg_plain);
					let selected =
						sel.is_some_and(|(a, b)| (line_idx, ci) >= a && (line_idx, ci) < b);
					if selected {
						canvas.fill(
							x,
							y,
							x + width as i32 * cw,
							y + ch,
							canvas::rgb(
								canvas::SELECTION_BG.0,
								canvas::SELECTION_BG.1,
								canvas::SELECTION_BG.2,
							),
						);
					}
					if is_cursor {
						canvas.fill(
							x,
							y,
							x + width as i32 * cw,
							y + ch,
							canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
						);
						fg = canvas::rgb(10, 12, 16);
					}
					if *c == ' ' && show_whitespace && !is_cursor {
						// The middle dot of the reference editor.
						let dx = x + cw / 2 - 1;
						let dy = y + ch * 62 / 100;
						canvas.fill(dx, dy, dx + 2, dy + 2, whitespace);
					}
					let mut buf = [0u8; 4];
					let s = c.encode_utf8(&mut buf);
					canvas.text(x, y, s, fg);
				}
				x_cells += char_cells(*c);
			}
			let at_line_end =
				focused && show_cursor && self.cursor.0 == line_idx && self.cursor.1 >= line.len();
			if at_line_end {
				let screen_col = x_cells.saturating_sub(self.hscroll);
				if screen_col < cols {
					let x = text_left + screen_col as i32 * cw;
					canvas.fill(
						x,
						y,
						x + cw,
						y + ch,
						canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
					);
				}
			}
			// A line break sits at the end of every line except the last.
			if show_whitespace && !at_line_end && line_idx + 1 < self.lines.len() {
				if x_cells >= self.hscroll {
					let screen_col = x_cells - self.hscroll;
					if screen_col < cols {
						let x = text_left + screen_col as i32 * cw + 2;
						let yy = y + ch * 55 / 100;
						canvas.line(x, yy, x, yy + ch / 5, whitespace);
						canvas.line(x - ch / 3, yy + ch / 5, x, yy + ch / 5, whitespace);
					}
				}
			}
		}
		// Diagnostic underlines, as vim.diagnostic draws them under the range.
		for d in &self.diags {
			let first = (d.start.line as usize).max(self.scroll);
			let last = (d.end.line as usize).min(self.scroll + rows.saturating_sub(1));
			for line_idx in first..=last {
				let Some(line) = self.lines.get(line_idx) else {
					break;
				};
				let from = if d.start.line as usize == line_idx {
					lsp::char_col(line, d.start.character)
				} else {
					0
				};
				let to = if d.end.line as usize == line_idx {
					lsp::char_col(line, d.end.character)
				} else {
					line.len()
				};
				let to = if to <= from { (from + 1).min(line.len().max(from + 1)) } else { to };
				let cell0 = cells_before(line, from).saturating_sub(self.hscroll);
				let cell1 = cells_before(line, to).saturating_sub(self.hscroll);
				let y = area.top + (line_idx - self.scroll) as i32 * ch;
				let px0 = text_left + cell0 as i32 * cw;
				let px1 = (text_left + cell1.max(cell0 + 1) as i32 * cw)
					.min(text_left + cols as i32 * cw);
				let color = canvas::rgb_tuple(severity_color(d.severity));
				if px1 > px0 {
					canvas.fill(px0, y + ch - 2, px1, y + ch - 1, color);
				}
			}
		}
		if footer == 1 {
			let y = area.top + (rows as i32) * ch;
			canvas.fill(
				area.left,
				y,
				area.left + area_cols as i32 * cw,
				y + ch,
				canvas::rgb(70, 20, 24),
			);
			let text: String = footer_text
				.as_deref()
				.unwrap_or("")
				.chars()
				.take(area_cols)
				.collect();
			canvas.text(area.left, y, &text, canvas::rgb(255, 200, 200));
		}
		if focused && show_cursor {
			if self
				.sig
				.as_ref()
				.is_some_and(|sig| sig.done && sig.help.is_some())
			{
				if let Some(sig) = &self.sig {
					draw_signature(
						canvas, area, cw, ch, text_left, cols, rows, self.scroll, self.cursor,
						sig,
					);
				}
			}
			if let Some(popup) = &self.completion {
				let filtered = self.completion_filtered();
				if !filtered.is_empty() {
					draw_completion(
						canvas, area, cw, ch, text_left, cols, area_cols, rows, self.scroll,
						self.hscroll, self.cursor, &self.lines, popup, &filtered,
					);
				}
			}
		}
		canvas.unclip(saved);
	}
}

fn run_git(root: &std::path::Path, args: &[&str]) -> Option<String> {
	let mut cmd = std::process::Command::new("git");
	cmd.arg("-C").arg(root).args(args);
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		// Started from a GUI process: no console window may flash.
		cmd.creation_flags(0x0800_0000);
	}
	let out = cmd.output().ok()?;
	if !out.status.success() {
		return None;
	}
	String::from_utf8(out.stdout).ok()
}

fn git_repo(path: &std::path::Path) -> Option<(PathBuf, String)> {
	let parent = path.parent()?;
	let text = run_git(parent, &["rev-parse", "--show-toplevel"])?;
	let root = std::fs::canonicalize(text.trim()).ok()?;
	let file = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
	let rel = file.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
	Some((root, rel))
}

/// Hunk headers of `git diff -U0`, `(old_start, old_count, new_start, new_count)`.
fn parse_hunks(diff: &str) -> Vec<(usize, usize, usize, usize)> {
	let parse = |text: &str| -> Option<(usize, usize)> {
		let text = text
			.strip_prefix('-')
			.or_else(|| text.strip_prefix('+'))?;
		let (start, count) = match text.split_once(',') {
			Some((start, count)) => (start, count),
			None => (text, "1"),
		};
		Some((start.parse().ok()?, count.parse().ok()?))
	};
	diff.lines()
		.filter(|line| line.starts_with("@@ "))
		.filter_map(|line| {
			let mut parts = line.split_whitespace();
			parts.next();
			let (old_start, old_count) = parse(parts.next()?)?;
			let (new_start, new_count) = parse(parts.next()?)?;
			Some((old_start, old_count, new_start, new_count))
		})
		.collect()
}

/// Gitsigns' default comparison: the working tree against the index. A
/// changed hunk marks its new lines, an added hunk marks all its lines, and a
/// pure deletion marks the line the deletion starts after.
fn git_signs(root: &std::path::Path, rel: &str, line_count: usize) -> Vec<Option<GitSign>> {
	let mut signs = vec![None; line_count];
	let Some(diff) = run_git(root, &["diff", "--no-color", "--unified=0", "--", rel]) else {
		return signs;
	};
	for (_, old_count, new_start, new_count) in parse_hunks(&diff) {
		if new_count == 0 {
			let row = new_start.saturating_sub(1);
			if row < line_count {
				signs[row] = Some(GitSign::Delete);
			}
			continue;
		}
		let kind = if old_count == 0 { GitSign::Add } else { GitSign::Change };
		let first = new_start.saturating_sub(1);
		for slot in signs.iter_mut().skip(first).take(new_count) {
			if *slot != Some(GitSign::Add) {
				*slot = Some(kind);
			}
		}
	}
	signs
}

fn sign_color(sign: GitSign) -> windows_sys::Win32::Foundation::COLORREF {
	match sign {
		GitSign::Add => canvas::rgb(166, 227, 161),
		GitSign::Change => canvas::rgb(249, 226, 175),
		GitSign::Delete => canvas::rgb(243, 139, 168),
	}
}

fn severity_name(severity: u8) -> &'static str {
	match severity {
		1 => "error",
		2 => "warning",
		3 => "info",
		_ => "hint",
	}
}

fn severity_color(severity: u8) -> (u8, u8, u8) {
	match severity {
		1 => (243, 139, 168),
		2 => (249, 226, 175),
		3 => (137, 220, 235),
		_ => (148, 226, 213),
	}
}

fn cells_before(line: &[char], col: usize) -> usize {
	let mut cell = 0usize;
	for c in line.iter().take(col) {
		cell += if *c == '\t' {
			4 - cell % 4
		} else {
			char_cells(*c)
		};
	}
	cell
}

/// Blink's `kind_icons` (`vendor/blink-cmp-1.8.0/lua/blink/cmp/config/appearance.lua`)
/// and catppuccin's per-kind colors (`.../integrations/blink_cmp.lua`).
fn kind_icon(kind: u8) -> (&'static str, (u8, u8, u8)) {
	const GREEN: (u8, u8, u8) = (166, 227, 161);
	const BLUE: (u8, u8, u8) = (137, 180, 250);
	const YELLOW: (u8, u8, u8) = (249, 226, 175);
	const PEACH: (u8, u8, u8) = (250, 179, 135);
	const MAUVE: (u8, u8, u8) = (203, 166, 247);
	const FLAMINGO: (u8, u8, u8) = (242, 205, 205);
	const RED: (u8, u8, u8) = (243, 139, 168);
	const TEAL: (u8, u8, u8) = (148, 226, 213);
	const SKY: (u8, u8, u8) = (137, 220, 235);
	const MAROON: (u8, u8, u8) = (235, 160, 172);
	match kind {
		1 => ("󰉿", GREEN),
		2 => ("󰊕", BLUE),
		3 => ("󰊕", BLUE),
		4 => ("󰒓", BLUE),
		5 => ("󰜢", GREEN),
		6 => ("󰆦", FLAMINGO),
		7 => ("󱡠", YELLOW),
		8 => ("󱡠", YELLOW),
		9 => ("󰅩", BLUE),
		10 => ("󰖷", BLUE),
		11 => ("󰪚", GREEN),
		12 => ("󰦨", PEACH),
		13 => ("󰦨", YELLOW),
		14 => ("󰻾", MAUVE),
		15 => ("󱄽", FLAMINGO),
		16 => ("󰏘", RED),
		17 => ("󰈔", BLUE),
		18 => ("󰬲", RED),
		19 => ("󰉋", BLUE),
		20 => ("󰦨", TEAL),
		21 => ("󰏿", PEACH),
		22 => ("󱡠", BLUE),
		23 => ("󱐋", BLUE),
		24 => ("󰪚", SKY),
		25 => ("󰬛", MAROON),
		// Blink falls back to the Field icon for an unknown kind.
		_ => ("󰜢", GREEN),
	}
}

/// The completion menu the reference draws: no border, one cell of padding,
/// a kind icon column, then the label with its detail in `PmenuExtra`.
#[allow(clippy::too_many_arguments)]
fn draw_completion(
	canvas: &Canvas,
	area: Rect,
	cw: i32,
	ch: i32,
	text_left: i32,
	cols: usize,
	area_cols: usize,
	rows: usize,
	scroll: usize,
	hscroll: usize,
	cursor: (usize, usize),
	lines: &[Vec<char>],
	popup: &Completion,
	filtered: &[usize],
) {
	let shown = filtered.len().min(10).min(rows.max(1));
	let offset = if popup.index >= shown {
		popup.index + 1 - shown
	} else {
		0
	};
	let item_label = |item: &lsp::Item| -> String {
		let mut label = item.label.clone();
		if item.kind == 15 {
			label.push('~');
		}
		label
	};
	// 1 padding + 1 icon + 1 gap + label + 1 gap + detail + 1 padding.
	let mut width = 15usize;
	for &index in filtered.iter().skip(offset).take(shown) {
		let item = &popup.items[index];
		let label: usize = item_label(item).chars().map(char_cells).sum();
		let detail: usize = item.detail.chars().map(char_cells).sum();
		width = width.max(5 + label + detail);
	}
	let width = width.min(cols.max(1));
	let cursor_row = cursor.0.saturating_sub(scroll);
	let cursor_cell = lines
		.get(cursor.0)
		.map(|line| cells_before(line, cursor.1))
		.unwrap_or(0)
		.saturating_sub(hscroll);
	let left_cells = cursor_cell.min(cols.saturating_sub(width));
	let top = if cursor_row + 1 + shown <= rows {
		area.top + (cursor_row as i32 + 1) * ch
	} else {
		area.top + (cursor_row.saturating_sub(shown) as i32) * ch
	}
	.max(area.top);
	let left = text_left + left_cells as i32 * cw;
	let right = left + width as i32 * cw;
	let _ = area_cols;
	canvas.fill(left, top, right, top + shown as i32 * ch, canvas::rgb(49, 50, 68));
	let label_fg = canvas::rgb(147, 153, 178);
	let detail_fg = canvas::rgb(108, 112, 134);
	for (slot, &index) in filtered.iter().skip(offset).take(shown).enumerate() {
		let item = &popup.items[index];
		let y = top + slot as i32 * ch;
		let selected = index == filtered.get(popup.index).copied().unwrap_or(usize::MAX);
		if selected {
			canvas.fill(left, y, right, y + ch, canvas::rgb(69, 71, 90));
		}
		let (icon, icon_color) = kind_icon(item.kind);
		canvas.text(left + cw, y, icon, canvas::rgb(icon_color.0, icon_color.1, icon_color.2));
		let label = item_label(item);
		let label_cells: usize = label.chars().map(char_cells).sum();
		canvas.text(left + 3 * cw, y, &label, label_fg);
		if !item.detail.is_empty() {
			let x = left + (4 + label_cells) as i32 * cw;
			if x < right {
				canvas.text(x, y, &item.detail, detail_fg);
			}
		}
	}
}

/// The signature window: the active signature's label, above the cursor when
/// there is room, with the active parameter on a `surface0` patch.
#[allow(clippy::too_many_arguments)]
fn draw_signature(
	canvas: &Canvas,
	area: Rect,
	cw: i32,
	ch: i32,
	text_left: i32,
	cols: usize,
	rows: usize,
	scroll: usize,
	cursor: (usize, usize),
	sig: &Signature,
) {
	let Some(help) = &sig.help else {
		return;
	};
	let mut index = help.active_signature;
	if index >= help.signatures.len() {
		index = 0;
	}
	let Some(active) = help.signatures.get(index) else {
		return;
	};
	let segments: Vec<&str> = active.label.split('\n').collect();
	let mut width_cells = segments
		.iter()
		.map(|line| line.chars().map(char_cells).sum::<usize>())
		.max()
		.unwrap_or(0)
		+ 2;
	width_cells = width_cells.min(100).min(cols.max(1));
	let height = segments.len().min(10).min(rows.max(1));
	let cursor_row = cursor.0.saturating_sub(scroll);
	let top_cells = if cursor_row >= height {
		cursor_row - height
	} else if cursor_row + 1 + height <= rows {
		cursor_row + 1
	} else {
		0
	};
	let cursor_cell = 0usize; // the window starts at the text's left edge
	let left = text_left + cursor_cell as i32 * cw;
	let top = area.top + top_cells as i32 * ch;
	let right = left + width_cells as i32 * cw;
	canvas.fill(left, top, right, top + height as i32 * ch, canvas::rgb(30, 30, 46));
	let fg = canvas::rgb(205, 214, 244);
	let patch = canvas::rgb(49, 50, 68);
	let mut line_start = 0usize;
	for (row, line) in segments.iter().take(height).enumerate() {
		let y = top + row as i32 * ch;
		let line_len = line.chars().count();
		if let Some((from, to)) = sig.range {
			let from = from.max(line_start);
			let to = to.min(line_start + line_len);
			if to > from {
				let x0 = left + cw + (from - line_start) as i32 * cw;
				let x1 = left + cw + (to - line_start) as i32 * cw;
				canvas.fill(x0, y, x1, y + ch, patch);
			}
		}
		canvas.text(left + cw, y, line, fg);
		line_start += line_len + 1;
	}
}

impl langconf::Model for Editor {
	fn line(&self, line: usize) -> String {
		let Some(row) = line.checked_sub(1) else {
			return String::new();
		};
		self.lines
			.get(row)
			.map(|chars| chars.iter().collect())
			.unwrap_or_default()
	}

	fn processed_line(&self, line: usize) -> String {
		let Some(row) = line.checked_sub(1) else {
			return String::new();
		};
		let Some(chars) = self.lines.get(row) else {
			return String::new();
		};
		match langconf::configuration(self.lang) {
			Some(config) => self.processed_row(row, 0, chars.len(), config),
			None => chars.iter().collect(),
		}
	}

	fn processed_before(&self, line: usize, column: usize) -> String {
		let Some(row) = line.checked_sub(1) else {
			return String::new();
		};
		let Some(chars) = self.lines.get(row) else {
			return String::new();
		};
		let take = column.saturating_sub(1).min(chars.len());
		match langconf::configuration(self.lang) {
			Some(config) => self.processed_row(row, 0, take, config),
			None => chars[..take].iter().collect(),
		}
	}

	fn processed_after(&self, line: usize, column: usize) -> String {
		let Some(row) = line.checked_sub(1) else {
			return String::new();
		};
		let Some(chars) = self.lines.get(row) else {
			return String::new();
		};
		let skip = column.saturating_sub(1).min(chars.len());
		match langconf::configuration(self.lang) {
			Some(config) => self.processed_row(row, skip, chars.len() - skip, config),
			None => chars[skip..].iter().collect(),
		}
	}

	fn scope_before(&self, line: usize, column: usize) -> ScopeKind {
		if column < 2 {
			return ScopeKind::Other;
		}
		self.scopes
			.get(line - 1)
			.and_then(|scopes| scopes.get(column - 2))
			.copied()
			.unwrap_or(ScopeKind::Other)
	}

	fn unmatched_closing_after(&self, line: usize, column: usize, open: &str) -> bool {
		self.scan_unmatched_close(line, column, open)
	}
}

fn starts_with(chars: &[char], at: usize, pattern: &[char]) -> bool {
	chars.len() >= at + pattern.len() && chars[at..at + pattern.len()] == *pattern
}

/// tree-sitter 节点对应的“字符串/注释/正则”作用域；嵌入表达式节点（f-string 的
/// interpolation、模板字符串的 template_substitution）重新标成 Other。
fn scope_of(node: tree_sitter::Node) -> Option<ScopeKind> {
	let kind = node.kind();
	if kind.contains("comment") {
		return Some(ScopeKind::Comment);
	}
	if kind.contains("string") || kind.contains("char_literal") || kind.contains("raw_string") {
		return Some(ScopeKind::String);
	}
	if kind == "regex" {
		return Some(ScopeKind::Regex);
	}
	if kind == "interpolation" || kind == "template_substitution" {
		return Some(ScopeKind::Other);
	}
	None
}

/// Screen cells one character occupies; tabs are handled by the caller.
pub fn char_cells(c: char) -> usize {
	c.width().unwrap_or(1).max(1)
}

fn hl_color(
	class: Hl,
	plain: windows_sys::Win32::Foundation::COLORREF,
) -> windows_sys::Win32::Foundation::COLORREF {
	match class {
		Hl::Plain => plain,
		// Comment → overlay2
		Hl::Comment => canvas::rgb(147, 153, 178),
		// String → green
		Hl::String => canvas::rgb(166, 227, 161),
		// @string.regexp → pink
		Hl::Regex => canvas::rgb(245, 194, 231),
		// Number / Boolean → peach
		Hl::Number => canvas::rgb(250, 179, 135),
		// Keyword / Include / Exception / Macro → mauve
		Hl::Keyword => canvas::rgb(203, 166, 247),
		// Conditional / Repeat → mauve
		Hl::Control => canvas::rgb(203, 166, 247),
		// Function → blue
		Hl::Function => canvas::rgb(137, 180, 250),
		// Type / Constructor → yellow
		Hl::Type => canvas::rgb(249, 226, 175),
		// Constant / @constant.builtin → peach
		Hl::Constant => canvas::rgb(250, 179, 135),
		// @variable / @variable.member → text
		Hl::Variable => canvas::rgb(205, 214, 244),
		// @variable.parameter → maroon
		Hl::Param => canvas::rgb(235, 160, 172),
		// @variable.member / @property → lavender
		Hl::Member => canvas::rgb(180, 190, 254),
		// @operator → sky
		Hl::Operator => canvas::rgb(137, 220, 235),
		// @punctuation.bracket / @punctuation.delimiter → overlay2
		Hl::Punct => canvas::rgb(147, 153, 178),
		// @variable.builtin (self / this / super) → red
		Hl::Builtin => canvas::rgb(243, 139, 168),
		// @string.escape → pink
		Hl::Escape => canvas::rgb(245, 194, 231),
		// @label → sapphire
		Hl::Label => canvas::rgb(116, 199, 236),
	}
}

fn is_identifier_kind(kind: &str) -> bool {
	matches!(
		kind,
		"identifier"
			| "type_identifier"
			| "field_identifier"
			| "property_identifier"
			| "shorthand_property_identifier"
			| "shorthand_property_identifier_pattern"
			| "private_property_identifier"
			| "statement_identifier"
	)
}

/// `keyword.control` 归 Control，其余关键字归 Keyword；两组的词表来自 VS Code 安装目录里的
/// `rust.tmLanguage.json`、`c.tmLanguage.json`、`MagicPython.tmLanguage.json`、
/// `JavaScript.tmLanguage.json`、`JSON.tmLanguage.json` 的 scope 与 match。
fn keyword_class(lang: Lang, text: &str) -> Option<Hl> {
	let control = match lang {
		Lang::Rust => matches!(
			text,
			"await" | "break" | "continue" | "do" | "else" | "for" | "if" | "loop" | "match"
				| "return" | "try" | "while" | "yield"
		),
		Lang::C => matches!(
			text,
			"break" | "continue" | "do" | "else" | "for" | "goto" | "if" | "return" | "while"
				| "switch" | "case" | "default" | "defined"
		),
		Lang::Python => matches!(
			text,
			"async" | "await" | "continue" | "del" | "assert" | "break" | "finally" | "for"
				| "from" | "elif" | "else" | "if" | "except" | "pass" | "raise" | "return"
				| "try" | "while" | "with" | "yield" | "import" | "as"
		),
		Lang::JavaScript => matches!(
			text,
			"break" | "continue" | "do" | "else" | "for" | "if" | "while" | "switch" | "case"
				| "default" | "try" | "catch" | "finally" | "throw" | "with" | "await" | "import"
				| "export" | "from" | "as" | "package"
		),
		Lang::Json | Lang::Plain => false,
	};
	if control {
		return Some(Hl::Control);
	}
	// `@boolean` 与 `@constant.builtin` 在 Catppuccin 里是 peach，跟数字同色。
	let constant = match lang {
		Lang::Rust => matches!(text, "true" | "false"),
		Lang::C => matches!(text, "NULL" | "TRUE" | "FALSE" | "true" | "false"),
		Lang::Python => matches!(text, "True" | "False" | "None" | "NotImplemented" | "Ellipsis"),
		Lang::JavaScript => {
			matches!(text, "true" | "false" | "null" | "undefined" | "NaN" | "Infinity")
		}
		Lang::Json => matches!(text, "true" | "false" | "null"),
		Lang::Plain => false,
	};
	if constant {
		return Some(Hl::Constant);
	}
	let keyword = match lang {
		Lang::Rust => matches!(
			text,
			"as" | "async" | "become" | "box" | "const" | "crate" | "dyn" | "extern" | "final"
				| "fn" | "gen" | "impl" | "in" | "let" | "macro" | "mod" | "move" | "mut"
				| "override" | "priv" | "pub" | "ref" | "static" | "struct" | "enum" | "trait"
				| "type" | "union" | "unsafe" | "unsized" | "use" | "virtual" | "where" | "self"
				| "Self" | "super"
		),
		Lang::C => matches!(
			text,
			"auto" | "const" | "extern" | "register" | "restrict" | "static" | "volatile"
				| "inline" | "typedef" | "struct" | "union" | "enum" | "int" | "char" | "short"
				| "long" | "float" | "double" | "signed" | "unsigned" | "void" | "bool" | "_Bool"
				| "sizeof" | "#include"
				| "#include_next" | "#import" | "#define" | "#undef" | "#if" | "#ifdef" | "#ifndef"
				| "#elif" | "#elifdef" | "#elifndef" | "#else" | "#endif" | "#pragma" | "#line"
				| "#error" | "#warning"
		),
		Lang::Python => matches!(
			text,
			"def" | "class" | "lambda" | "global" | "nonlocal" | "and" | "or" | "not" | "is"
				| "in"
		),
		Lang::JavaScript => matches!(
			text,
			"var" | "let" | "const" | "function" | "class" | "extends" | "static" | "async"
				| "get" | "set" | "accessor" | "declare" | "abstract" | "override" | "public"
				| "protected" | "private" | "readonly" | "new" | "typeof" | "instanceof" | "void"
				| "delete" | "in" | "of" | "this" | "super" | "debugger" | "arguments"
		),
		Lang::Json => false,
		Lang::Plain => false,
	};
	keyword.then_some(Hl::Keyword)
}

fn field_is(parent: tree_sitter::Node, field: &str, node: tree_sitter::Node) -> bool {
	parent
		.child_by_field_name(field)
		.is_some_and(|f| f.id() == node.id())
}

fn is_upper_snake(text: &str) -> bool {
	text.chars().any(|c| c.is_ascii_uppercase())
		&& text
			.chars()
			.all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
}

fn classify(node: tree_sitter::Node, source: &[u8], lang: Lang) -> Hl {
	let kind = node.kind();
	let text = node.utf8_text(source).unwrap_or("");
	if lang == Lang::Json && is_json_key(node) {
		return Hl::Member;
	}
	if kind.contains("comment") {
		return Hl::Comment;
	}
	if kind == "regex" {
		return Hl::Regex;
	}
	if kind == "escape_sequence" || kind == "escape" {
		return Hl::Escape;
	}
	if kind.contains("string") || kind.contains("char_literal") || kind.contains("raw_string") {
		return Hl::String;
	}
	if kind == "number_literal"
		|| kind == "number"
		|| kind.contains("integer")
		|| kind.contains("float")
	{
		return Hl::Number;
	}
	// `#define FOO 1` 的数字在 preproc_arg 里（meta.preprocessor.numeric）。
	if kind == "preproc_arg" && (text.parse::<u64>().is_ok() || text.parse::<f64>().is_ok()) {
		return Hl::Number;
	}
	// Leaves such as `+`, `->` and `(` carry no children of their own.
	if let Some(class) = operator_class(kind) {
		return class;
	}
	if !is_identifier_kind(kind) {
		if let Some(class) = keyword_class(lang, text) {
			return class;
		}
	}
	match lang {
		Lang::Rust => classify_rust(node, text),
		Lang::C => classify_c(node, text),
		Lang::Python => classify_python(node, text),
		Lang::JavaScript => classify_js(node, text),
		Lang::Json | Lang::Plain => Hl::Plain,
	}
}

/// JSON 的键是 `pair` 的 `key` 字段那个字符串（`string.json support.type.property-name.json`）。
fn is_json_key(node: tree_sitter::Node) -> bool {
	if node.kind() == "string" {
		return node.parent().is_some_and(|p| {
			p.kind() == "pair" && p.child_by_field_name("key").is_some_and(|k| k.id() == node.id())
		});
	}
	if matches!(node.kind(), "string_content" | "escape_sequence") {
		return node.parent().is_some_and(is_json_key);
	}
	false
}

/// Operator and punctuation leaves: nvim-treesitter captures them as
/// `@operator` (sky) and `@punctuation.bracket` / `@punctuation.delimiter`
/// (overlay2).
fn operator_class(kind: &str) -> Option<Hl> {
	match kind {
		"," | ";" | "." | "::" | "(" | ")" | "[" | "]" | "{" | "}" => Some(Hl::Punct),
		"+" | "-" | "*" | "/" | "%" | "=" | "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&"
		| "||" | "!" | "&" | "|" | "^" | "~" | "<<" | ">>" | "->" | "+=" | "-=" | "*="
		| "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=" | "++" | "--" | "?" | ":" | "=>" => {
			Some(Hl::Operator)
		}
		_ => None,
	}
}

/// Whether an identifier sits in a parameter list, the `@variable.parameter`
/// capture.
fn is_parameter(node: tree_sitter::Node) -> bool {
	let mut node = node;
	for _ in 0..4 {
		let Some(parent) = node.parent() else {
			return false;
		};
		match parent.kind() {
			"parameter" | "parameters" | "formal_parameters" | "parameter_declaration"
			| "typed_parameter" | "default_parameter" | "typed_default_parameter"
			| "optional_parameter" => return true,
			// 参数注解里的类型名不是参数本身（`x: int` 的 int）。
			"type" | "type_annotation" => return false,
			_ => node = parent,
		}
	}
	false
}

fn classify_rust(node: tree_sitter::Node, text: &str) -> Hl {
	match node.kind() {
		"type_identifier" | "primitive_type" | "lifetime" | "label" => Hl::Type,
		"field_identifier" => Hl::Member,
		"identifier" => {
			if text == "self" {
				return Hl::Builtin;
			}
			if is_parameter(node) {
				return Hl::Param;
			}
			let Some(parent) = node.parent() else {
				return Hl::Variable;
			};
			match parent.kind() {
				"function_item" | "function_signature_item" if field_is(parent, "name", node) => {
					Hl::Function
				}
				"call_expression" if field_is(parent, "function", node) => Hl::Function,
				"macro_invocation" if field_is(parent, "macro", node) => Hl::Function,
				"enum_variant" if field_is(parent, "name", node) => Hl::Constant,
				"scoped_identifier" => {
					// `String::new()`：name 是调用名，前面的 path 是类型或模块。
					if field_is(parent, "name", node) {
						if parent.parent().is_some_and(|g| {
							g.kind() == "call_expression" && field_is(g, "function", parent)
						}) {
							return Hl::Function;
						}
					}
					Hl::Type
				}
				_ => {
					if matches!(text, "Some" | "None" | "Ok" | "Err") {
						return Hl::Type;
					}
					if is_upper_snake(text) {
						return Hl::Constant;
					}
					Hl::Variable
				}
			}
		}
		_ => Hl::Plain,
	}
}

fn classify_c(node: tree_sitter::Node, text: &str) -> Hl {
	match node.kind() {
		"type_identifier" => Hl::Type,
		"field_identifier" => Hl::Member,
		"sized_type_specifier" => Hl::Keyword,
		"statement_identifier" => Hl::Label,
		"identifier" => {
			if matches!(text, "NULL" | "TRUE" | "FALSE" | "true" | "false") {
				return Hl::Keyword;
			}
			if is_parameter(node) {
				return Hl::Param;
			}
			let Some(parent) = node.parent() else {
				return Hl::Variable;
			};
			match parent.kind() {
				"call_expression" if field_is(parent, "function", node) => Hl::Function,
				// `entity.name.function.preprocessor` 在 dark_vs 里是 #569CD6，
				// 跟 `#define` 同一个蓝，不是函数的黄。
				"preproc_def" | "preproc_function_def" if field_is(parent, "name", node) => {
					Hl::Keyword
				}
				"enumerator" => Hl::Constant,
				_ if is_declarator_name(node) => Hl::Function,
				_ => Hl::Variable,
			}
		}
		_ => Hl::Plain,
	}
}

/// `function_declarator` 的 declarator 可能隔着一层 `pointer_declarator` 等。
fn is_declarator_name(mut node: tree_sitter::Node) -> bool {
	for _ in 0..8 {
		let Some(parent) = node.parent() else {
			return false;
		};
		match parent.kind() {
			"pointer_declarator" | "parenthesized_declarator" | "attributed_declarator" => {
				if !field_is(parent, "declarator", node) {
					return false;
				}
				node = parent;
			}
			"function_declarator" => return field_is(parent, "declarator", node),
			_ => return false,
		}
	}
	false
}

fn classify_python(node: tree_sitter::Node, text: &str) -> Hl {
	if node.kind() != "identifier" {
		return Hl::Plain;
	}
	if text == "self" {
		return Hl::Builtin;
	}
	if is_parameter(node) {
		return Hl::Param;
	}
	if let Some(parent) = node.parent() {
		match parent.kind() {
			"function_definition" if field_is(parent, "name", node) => return Hl::Function,
			"class_definition" if field_is(parent, "name", node) => return Hl::Type,
			"decorator" => return Hl::Function,
			// `Foo.str` 的 str 是属性，即使叫 str 也不按内置类型上色。
			"attribute" if field_is(parent, "attribute", node) => return Hl::Member,
			_ => {}
		}
	}
	// support.type.python 比 meta.function-call 更具体：`str(x)` 里的 str 是类型色。
	if is_python_type(text) {
		return Hl::Type;
	}
	if node
		.parent()
		.is_some_and(|p| p.kind() == "call" && field_is(p, "function", node))
	{
		return Hl::Function;
	}
	Hl::Variable
}

/// `support.type.python` 与 `support.type.exception.python` 的词表。
fn is_python_type(text: &str) -> bool {
	matches!(
		text,
		"bool" | "bytearray" | "bytes" | "classmethod" | "complex" | "dict" | "float"
			| "frozenset" | "int" | "list" | "object" | "property" | "set" | "slice"
			| "staticmethod" | "str" | "tuple" | "type" | "super"
	) || text.ends_with("Error")
		|| text.ends_with("Warning")
		|| matches!(
			text,
			"BaseException"
				| "Exception"
				| "GeneratorExit"
				| "KeyboardInterrupt"
				| "SystemExit"
				| "StopIteration"
				| "StopAsyncIteration"
		)
}

fn classify_js(node: tree_sitter::Node, text: &str) -> Hl {
	match node.kind() {
		"this" | "super" => Hl::Builtin,
		"property_identifier"
		| "shorthand_property_identifier"
		| "shorthand_property_identifier_pattern"
		| "private_property_identifier" => {
			if let Some(parent) = node.parent() {
				if parent.kind() == "method_definition" && field_is(parent, "name", node) {
					return Hl::Function;
				}
				if parent.kind() == "member_expression"
					&& field_is(parent, "property", node)
					&& parent.parent().is_some_and(|g| {
						g.kind() == "call_expression" && field_is(g, "function", parent)
					})
				{
					return Hl::Function;
				}
			}
			Hl::Member
		}
		"statement_identifier" => Hl::Label,
		"identifier" => {
			if is_parameter(node) {
				return Hl::Param;
			}
			if let Some(parent) = node.parent() {
				match parent.kind() {
					"function_declaration"
					| "function_expression"
					| "generator_function_declaration"
					| "generator_function"
						if field_is(parent, "name", node) =>
					{
						return Hl::Function;
					}
					"call_expression" if field_is(parent, "function", node) => {
						return Hl::Function;
					}
					"class_declaration" | "class" if field_is(parent, "name", node) => {
						return Hl::Type;
					}
					"class_heritage" => return Hl::Type,
					"new_expression" if field_is(parent, "constructor", node) => return Hl::Type,
					_ => {}
				}
			}
			if is_upper_snake(text) {
				Hl::Constant
			} else {
				Hl::Variable
			}
		}
		_ => Hl::Plain,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn editor(content: &str) -> Editor {
		Editor::new(PathBuf::from("test.rs"), content.to_string(), false)
	}

	#[test]
	fn replace_and_undo_do_not_panic_highlight() {
		let mut ed = editor("fn main() {\n\tlet x = 1;\n}\n");
		assert_eq!(ed.hl.len(), 4);
		ed.replace_all("// short");
		assert_eq!(ed.text(), "// short");
		assert_eq!(ed.hl.len(), 1);
		ed.replace_all("fn again() -> bool { true }");
		assert_eq!(ed.text(), "fn again() -> bool { true }");
		ed.undo();
		ed.undo();
		ed.redo();
	}

	#[test]
	fn undo_clears_selection_from_removed_line() {
		let mut ed = editor("a");
		ed.end(false);
		ed.split_line();
		ed.insert('b', false);
		ed.home(false);
		ed.end(true);
		ed.undo();
		ed.undo();
		assert_eq!(ed.text(), "a");
		assert_eq!(ed.copy(), "");
		ed.redo();
		assert!(ed.anchor.is_none());
	}

	#[test]
	fn accepting_a_snippet_selects_and_walks_its_placeholders() {
		let mut ed = editor("int main() {\n\tpr\n}");
		ed.cursor = (1, 3);
		let item = lsp::Item {
			label: String::from("printf"),
			detail: String::new(),
			filter_text: Some(String::from("printf")),
			sort_text: None,
			insert_text: None,
			text_edit: Some(lsp::Edit {
				start: lsp::Pos { line: 1, character: 1 },
				end: lsp::Pos { line: 1, character: 3 },
				new_text: String::from("printf(${1:int n}, ${2:char *s})"),
			}),
			additional: vec![lsp::Edit {
				start: lsp::Pos { line: 0, character: 0 },
				end: lsp::Pos { line: 0, character: 0 },
				new_text: String::from("#include <stdio.h>\n"),
			}],
			insert_format: 2,
			kind: 0,
		};
		ed.completion = Some(Completion {
			req: 0,
			items: vec![item],
			index: 0,
			incomplete: false,
			done: true,
			start: (1, 1),
		});
		ed.accept_completion(0);
		assert_eq!(ed.text(), "#include <stdio.h>\nint main() {\n\tprintf(int n, char *s)\n}");
		// The include edit adds a line above; the first placeholder stays on
		// printf's line and is selected.
		assert_eq!(ed.selection(), Some(((2, 8), (2, 13))));
		ed.key(&Key { vk: 0x58, ch: Some('x'), down: true, ..Default::default() });
		// Typing replaces the placeholder and the later stop shifts left.
		assert_eq!(ed.text(), "#include <stdio.h>\nint main() {\n\tprintf(x, char *s)\n}");
		ed.key(&Key { vk: 0x09, down: true, ..Default::default() });
		assert_eq!(ed.selection(), Some(((2, 11), (2, 18))));
		ed.key(&Key { vk: 0x09, down: true, ..Default::default() });
		assert_eq!(ed.text(), "#include <stdio.h>\nint main() {\n\tprintf(x, char *s\t)\n}");
		assert!(ed.snippet.is_none());
	}

	#[test]
	fn shift_selection_excludes_cursor_character() {
		let mut ed = editor("abc");
		ed.move_cursor(0, 1, true);
		assert_eq!(ed.copy(), "a");
		assert_eq!(ed.cut(), "a");
		assert_eq!(ed.text(), "bc");
		ed.undo();
		assert_eq!(ed.text(), "abc");
		assert!(ed.anchor.is_none());
	}

	#[test]
	fn edits_keep_highlight_in_sync() {
		let mut ed = editor("fn main() {}\n");
		for ch in "x let y".chars() {
			ed.insert(ch, false);
		}
		ed.highlight();
		assert!(ed.hl.iter().map(|l| l.len()).sum::<usize>() > 0);
		ed.backspace();
		ed.delete();
		ed.highlight();
	}

	#[test]
	fn enter_after_open_brace_indents() {
		let mut ed = editor("fn main() {");
		ed.end(false);
		ed.enter();
		assert_eq!(ed.text(), "fn main() {\n\t");
		assert_eq!(ed.cursor, (1, 1));
	}

	#[test]
	fn enter_between_parens_adds_outdented_line() {
		let mut ed = editor("foo()");
		ed.set_cursor(0, 4, false);
		ed.enter();
		assert_eq!(ed.text(), "foo(\n\t\n)");
		assert_eq!(ed.cursor, (1, 1));
	}

	#[test]
	fn enter_in_plain_text_keeps_indentation() {
		let mut ed = Editor::new(PathBuf::from("test.txt"), "  abc".to_string(), false);
		ed.end(false);
		ed.enter();
		assert_eq!(ed.text(), "  abc\n  ");
	}

	#[test]
	fn enter_replaces_selection() {
		let mut ed = editor("ab");
		ed.anchor = Some((0, 0));
		ed.cursor = (0, 2);
		ed.enter();
		assert_eq!(ed.text(), "\n");
		assert_eq!(ed.cursor, (1, 0));
	}

	#[test]
	fn enter_replaces_a_multi_line_selection() {
		let mut ed = editor("fn main() {\n\tlet x = 1;\n}\n");
		ed.anchor = Some((1, 1));
		ed.cursor = (2, 0);
		ed.enter();
		assert_eq!(ed.text(), "fn main() {\n\t\n}\n");
		assert_eq!(ed.cursor, (2, 0));
	}

	#[test]
	fn enter_drops_auto_closed_pairs_that_become_multi_line() {
		let mut ed = editor("");
		ed.type_char('(');
		ed.type_char('a');
		assert_eq!(ed.text(), "(a)");
		// 光标在 `a` 前回车：新行在括号对内，闭合符落到下一行。
		ed.cursor = (0, 1);
		ed.enter();
		assert_eq!(ed.text(), "(\n\ta)");
		assert!(ed.auto_closed.is_empty());
		// 括号对跨行后不再算数，再打 `)` 不会越过。
		ed.cursor = (1, 2);
		ed.type_char(')');
		assert_eq!(ed.text(), "(\n\ta))");
	}

	#[test]
	fn typing_bracket_auto_closes_and_overtypes() {
		let mut ed = editor("");
		ed.type_char('(');
		assert_eq!(ed.text(), "()");
		assert_eq!(ed.cursor, (0, 1));
		ed.type_char(')');
		assert_eq!(ed.text(), "()");
		assert_eq!(ed.cursor, (0, 2));
	}

	#[test]
	fn overtype_survives_moving_inside_the_pair() {
		let mut ed = editor("");
		ed.type_char('(');
		ed.type_char('a');
		ed.move_cursor(0, -1, false);
		ed.move_cursor(0, 1, false);
		assert_eq!(ed.text(), "(a)");
		ed.type_char(')');
		assert_eq!(ed.text(), "(a)");
		assert_eq!(ed.cursor, (0, 3));
	}

	#[test]
	fn overtype_is_dropped_after_leaving_the_pair() {
		let mut ed = editor("");
		ed.type_char('(');
		ed.move_cursor(0, -1, false);
		ed.move_cursor(0, 1, false);
		ed.type_char(')');
		assert_eq!(ed.text(), "())");
		assert_eq!(ed.cursor, (0, 2));
	}

	#[test]
	fn backspace_removes_auto_inserted_pair() {
		let mut ed = editor("");
		ed.type_char('[');
		assert_eq!(ed.text(), "[]");
		ed.backspace();
		assert_eq!(ed.text(), "");
		assert_eq!(ed.cursor, (0, 0));
	}

	#[test]
	fn quote_inside_string_does_not_auto_close() {
		let mut ed = editor("let s = \"\";");
		ed.set_cursor(0, 9, false);
		ed.type_char('"');
		assert_eq!(ed.text(), "let s = \"\"\";");
	}

	#[test]
	fn typing_quote_surrounds_selection() {
		let mut ed = editor("abc");
		ed.anchor = Some((0, 0));
		ed.cursor = (0, 3);
		ed.type_char('"');
		assert_eq!(ed.text(), "\"abc\"");
		assert_eq!(ed.copy(), "abc");
		assert_eq!(ed.cursor, (0, 4));
	}

	#[test]
	fn typing_bracket_surrounds_selection() {
		let mut ed = editor("abc");
		ed.anchor = Some((0, 0));
		ed.cursor = (0, 3);
		ed.type_char('(');
		assert_eq!(ed.text(), "(abc)");
		assert_eq!(ed.selection(), Some(((0, 1), (0, 4))));
	}

	#[test]
	fn backspace_inside_a_pair_keeps_it_overtypeable() {
		let mut ed = editor("");
		ed.type_char('(');
		ed.type_char('a');
		assert_eq!(ed.text(), "(a)");
		ed.backspace();
		assert_eq!(ed.text(), "()");
		assert_eq!(ed.cursor, (0, 1));
		ed.type_char(')');
		assert_eq!(ed.text(), "()");
		assert_eq!(ed.cursor, (0, 2));
	}

	#[test]
	fn backspace_removes_only_the_pair_under_the_cursor() {
		let mut ed = editor("");
		ed.type_char('(');
		ed.type_char('x');
		ed.backspace();
		assert_eq!(ed.text(), "()");
		assert_eq!(ed.cursor, (0, 1));
		ed.backspace();
		assert_eq!(ed.text(), "");
	}

	#[test]
	fn selection_delete_keeps_the_outer_pair_record() {
		let mut ed = editor("");
		ed.type_char('(');
		ed.type_char('x');
		ed.type_char('(');
		assert_eq!(ed.text(), "(x())");
		ed.anchor = Some((0, 1));
		ed.cursor = (0, 2);
		ed.backspace();
		assert_eq!(ed.text(), "(())");
		assert_eq!(ed.cursor, (0, 1));
		// 外层那对还在（光标严格落在里面）；内层那对因光标停在它的开括号上被作废。
		assert_eq!(ed.auto_closed_positions(), vec![(0, 3)]);
		ed.set_cursor(0, 3, false);
		ed.type_char(')');
		assert_eq!(ed.text(), "(())");
		assert_eq!(ed.cursor, (0, 4));
	}

	#[test]
	fn deleting_the_close_char_drops_the_pair_record() {
		let mut ed = editor("");
		ed.type_char('(');
		ed.type_char('a');
		ed.set_cursor(0, 2, false);
		ed.delete();
		assert_eq!(ed.text(), "(a");
		assert!(ed.auto_closed.is_empty());
	}

	// cursor.test.ts: issue #37315 - it can remember multiple auto-closed instances
	#[test]
	fn nested_pairs_overtype_one_at_a_time() {
		let mut ed = editor("");
		for ch in "x=(".chars() {
			ed.type_char(ch);
		}
		assert_eq!(ed.text(), "x=()");
		ed.type_char('(');
		assert_eq!(ed.text(), "x=(())");
		ed.type_char(')');
		assert_eq!(ed.text(), "x=(())");
		ed.type_char(')');
		assert_eq!(ed.text(), "x=(())");
		assert_eq!(ed.cursor, (0, 6));
	}

	// cursor.test.ts: issue #118270 - auto closing deletes only those characters that it inserted
	#[test]
	fn backspace_deletes_only_auto_inserted_characters() {
		let mut ed = editor("");
		for ch in "x=(".chars() {
			ed.type_char(ch);
		}
		for ch in "asd".chars() {
			ed.type_char(ch);
		}
		assert_eq!(ed.text(), "x=(asd)");
		for _ in 0..3 {
			ed.backspace();
		}
		assert_eq!(ed.text(), "x=()");
		ed.backspace();
		assert_eq!(ed.text(), "x=");

		let mut ed = editor("y=();");
		ed.set_cursor(0, 3, false);
		ed.backspace();
		assert_eq!(ed.text(), "y=);");
	}

	// cursor.test.ts: issue #78527 - does not close quote on odd count
	#[test]
	fn quote_closes_inside_brackets() {
		let mut ed = editor("std::cout << '\"' << entryMap");
		ed.end(false);
		for ch in "[\"a\"]".chars() {
			ed.type_char(ch);
		}
		assert_eq!(ed.text(), "std::cout << '\"' << entryMap[\"a\"]");
	}

	// cursor.test.ts: issue #27937 - Trying to add an item to the front of a list is cumbersome
	#[test]
	fn quote_before_an_existing_quote_does_not_overtype() {
		let mut ed = editor("var arr = [\"b\", \"c\"];");
		ed.set_cursor(0, 11, false);
		ed.type_char('"');
		assert_eq!(ed.text(), "var arr = [\"\"b\", \"c\"];");
	}

	// cursor.test.ts: issue #53357 - Over typing ignores characters after backslash
	// 单引号配对只出现在 Python 配置里（Rust 配置没有 `'`）。
	#[test]
	fn quote_after_backslash_does_not_overtype() {
		let mut ed = Editor::new(PathBuf::from("test.py"), "console.log();".to_string(), false);
		ed.set_cursor(0, 12, false);
		ed.type_char('\'');
		assert_eq!(ed.text(), "console.log('');");
		for ch in "it".chars() {
			ed.type_char(ch);
		}
		assert_eq!(ed.text(), "console.log('it');");
		ed.type_char('\\');
		assert_eq!(ed.text(), "console.log('it\\');");
		ed.type_char('\'');
		assert_eq!(ed.text(), "console.log('it\\'');");
	}

	// cursor.test.ts: issue #84998 - Overtyping Brackets doesn't work after backslash
	#[test]
	fn bracket_after_backslash_still_overtypes() {
		let mut ed = editor("");
		for ch in "\\(".chars() {
			ed.type_char(ch);
		}
		assert_eq!(ed.text(), "\\()");
		for ch in "abc".chars() {
			ed.type_char(ch);
		}
		assert_eq!(ed.text(), "\\(abc)");
		ed.type_char('\\');
		assert_eq!(ed.text(), "\\(abc\\)");
		ed.type_char(')');
		assert_eq!(ed.text(), "\\(abc\\)");
	}

	// cursor.test.ts: issue #6841 - Auto closing brackets should balance brackets
	#[test]
	fn auto_close_balances_brackets() {
		let cases = [
			("\n}", (0, 0), '{', "{\n}"),
			("", (0, 0), '{', "{}"),
			("{}", (0, 1), '{', "{{}}"),
			("{\n", (1, 0), '{', "{\n{}"),
			("\n]", (0, 0), '{', "{}\n]"),
			(")\n", (1, 0), '(', ")\n()"),
			(
				"function foo() {\n\n}",
				(1, 0),
				'{',
				"function foo() {\n{}\n}",
			),
			("someFunction);", (0, 12), '(', "someFunction();"),
			("someFunction;", (0, 12), '(', "someFunction();"),
			("someFunctionsomeParam);", (0, 12), '(', "someFunction(someParam);"),
		];
		for (text, cursor, ch, expected) in cases {
			let mut ed = editor(text);
			ed.set_cursor(cursor.0, cursor.1, false);
			ed.type_char(ch);
			assert_eq!(ed.text(), expected, "text={text:?} cursor={cursor:?} ch={ch:?}");
		}
	}

	fn hl_of(ed: &mut Editor, row: usize, needle: &str) -> Hl {
		ed.highlight();
		let text: String = ed.lines[row].iter().collect();
		let byte = text
			.find(needle)
			.unwrap_or_else(|| panic!("{needle:?} not in {text:?}"));
		let col = text[..byte].chars().count();
		ed.hl[row][col]
	}

	fn hl_at(ed: &mut Editor, row: usize, col: usize) -> Hl {
		ed.highlight();
		ed.hl[row][col]
	}

	// 颜色分组对应 Catppuccin Mocha 的 treesitter 捕获：
	// @comment/@punctuation 是 overlay2，@variable.parameter 是 maroon，
	// @variable.member 是 lavender，@operator 是 sky，@boolean 是 peach。
	#[test]
	fn rust_tokens_use_catppuccin_colors() {
		let mut ed = editor(
			"// c\nfn main() {\n\tlet x: i32 = 1;\n\tlet s = \"a\\n\";\n\tif x == 1 && true {}\n\tprintln!(\"{}\", Some(x));\n}\nconst MAX: u32 = 2;\n",
		);
		assert_eq!(hl_at(&mut ed, 0, 0), Hl::Comment);
		assert_eq!(hl_of(&mut ed, 1, "fn"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 1, "main"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 2, "let"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 2, "x"), Hl::Variable);
		assert_eq!(hl_of(&mut ed, 2, "i32"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 2, "1"), Hl::Number);
		assert_eq!(hl_of(&mut ed, 3, "s"), Hl::Variable);
		assert_eq!(hl_of(&mut ed, 3, "\\n"), Hl::Escape);
		assert_eq!(hl_of(&mut ed, 4, "if"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 4, "&&"), Hl::Operator);
		assert_eq!(hl_of(&mut ed, 4, "true"), Hl::Constant);
		assert_eq!(hl_of(&mut ed, 5, "println"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 5, "Some"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 7, "const"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 7, "MAX"), Hl::Constant);
	}

	#[test]
	fn string_end_is_not_painted_past_the_node() {
		let mut ed = editor("let s = \"a\";");
		assert_eq!(hl_at(&mut ed, 0, 8), Hl::String);
		assert_eq!(hl_at(&mut ed, 0, 10), Hl::String);
		assert_eq!(hl_at(&mut ed, 0, 11), Hl::Punct);
	}

	#[test]
	fn c_tokens_use_catppuccin_colors() {
		let mut ed = Editor::new(
			PathBuf::from("test.c"),
			"/* c */\n#include <stdio.h>\n#define FOO 1\ntypedef struct S { int a; };\nenum E { A };\nint main(void) {\n\tint x = foo(1);\n\tswitch (x) { case 1: break; default: break; }\n\tgoto done;\ndone:\n\treturn NULL;\n}\n#define ADD(a, b) ((a) + (b))\nint sum3(int a, int b);\n".to_string(),
			false,
		);
		assert_eq!(hl_at(&mut ed, 0, 0), Hl::Comment);
		assert_eq!(hl_at(&mut ed, 1, 0), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 1, "stdio"), Hl::String);
		assert_eq!(hl_of(&mut ed, 2, "#define"), Hl::Keyword);
		// 宏名跟 `#define` 同色。
		assert_eq!(hl_of(&mut ed, 2, "FOO"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 2, "1"), Hl::Number);
		assert_eq!(hl_of(&mut ed, 3, "typedef"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 3, "struct"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 3, "S"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 3, "int"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 3, "a"), Hl::Member);
		assert_eq!(hl_of(&mut ed, 4, "A"), Hl::Constant);
		assert_eq!(hl_of(&mut ed, 5, "main"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 5, "void"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 6, "x"), Hl::Variable);
		assert_eq!(hl_of(&mut ed, 6, "foo"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 7, "switch"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 7, "case"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 7, "break"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 8, "goto"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 8, "done"), Hl::Label);
		assert_eq!(hl_of(&mut ed, 10, "NULL"), Hl::Constant);
		assert_eq!(hl_of(&mut ed, 12, "ADD"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 12, "a"), Hl::Variable);
		// 函数声明里的参数是变量，不是函数名。
		assert_eq!(hl_of(&mut ed, 13, "sum3"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 13, "a"), Hl::Param);
		assert_eq!(hl_of(&mut ed, 13, "b"), Hl::Param);
	}

	#[test]
	fn python_tokens_use_catppuccin_colors() {
		let mut ed = Editor::new(
			PathBuf::from("test.py"),
			"# c\nclass Foo(object):\n\tdef bar(self, x: int = 1):\n\t\tif x and True:\n\t\t\tprint(Foo.str)\n\t\treturn str(x)\n".to_string(),
			false,
		);
		assert_eq!(hl_at(&mut ed, 0, 0), Hl::Comment);
		assert_eq!(hl_of(&mut ed, 1, "class"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 1, "Foo"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 1, "object"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 2, "def"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 2, "bar"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 2, "self"), Hl::Builtin);
		assert_eq!(hl_of(&mut ed, 2, "int"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 2, "1"), Hl::Number);
		assert_eq!(hl_of(&mut ed, 3, "if"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 3, "and"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 3, "True"), Hl::Constant);
		assert_eq!(hl_of(&mut ed, 4, "print"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 4, "str"), Hl::Member);
		assert_eq!(hl_of(&mut ed, 5, "return"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 5, "str"), Hl::Type);
	}

	#[test]
	fn javascript_tokens_use_catppuccin_colors() {
		let mut ed = Editor::new(
			PathBuf::from("test.js"),
			"// c\nimport {a} from 'm';\nexport const MAX = 1;\nfunction foo(x) { return new Bar().baz(x); }\nclass Bar extends Baz {}\nconst obj = { key: 1 };\nobj.method(2);\nlabel: for (;;) { break label; }\nconst re = /ab+c/g;\nconst s = 'a';\n".to_string(),
			false,
		);
		assert_eq!(hl_at(&mut ed, 0, 0), Hl::Comment);
		assert_eq!(hl_of(&mut ed, 1, "import"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 1, "from"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 1, "'m'"), Hl::String);
		assert_eq!(hl_of(&mut ed, 2, "export"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 2, "const"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 2, "MAX"), Hl::Constant);
		assert_eq!(hl_of(&mut ed, 3, "function"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 3, "foo"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 3, "new"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 3, "Bar"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 3, "baz"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 3, "x)"), Hl::Param);
		assert_eq!(hl_of(&mut ed, 4, "class"), Hl::Keyword);
		assert_eq!(hl_of(&mut ed, 4, "Baz"), Hl::Type);
		assert_eq!(hl_of(&mut ed, 5, "key"), Hl::Member);
		assert_eq!(hl_of(&mut ed, 6, "method"), Hl::Function);
		assert_eq!(hl_of(&mut ed, 7, "label:"), Hl::Label);
		assert_eq!(hl_of(&mut ed, 7, "break"), Hl::Control);
		assert_eq!(hl_of(&mut ed, 8, "ab+c"), Hl::Regex);
		assert_eq!(hl_of(&mut ed, 9, "'a'"), Hl::String);
	}

	#[test]
	fn json_tokens_use_catppuccin_colors() {
		let mut ed = Editor::new(
			PathBuf::from("test.json"),
			"{\"key\": \"value\", \"n\": 1, \"t\": true}\n".to_string(),
			false,
		);
		ed.highlight();
		assert_eq!(hl_at(&mut ed, 0, 2), Hl::Member);
		assert_eq!(hl_at(&mut ed, 0, 9), Hl::String);
		assert_eq!(hl_at(&mut ed, 0, 18), Hl::Member);
		assert_eq!(hl_of(&mut ed, 0, "1"), Hl::Number);
		assert_eq!(hl_of(&mut ed, 0, "true"), Hl::Constant);
	}
}
