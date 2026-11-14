//! solid: our editor. Capability shape from fresh/helix/nvim (buffer, selection,
//! undo, tree-sitter highlighting); no mode machine and no borrowed keys.

use std::path::PathBuf;

use tree_sitter::Parser;
use unicode_width::UnicodeWidthChar;

use crate::canvas::{self, Canvas};
use crate::layout::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hl {
	Plain,
	Keyword,
	String,
	Comment,
	Number,
	Function,
	Type,
	Constant,
	Property,
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
		};
		ed.highlight();
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
		self.clamp_cursor();
		self.highlight();
	}

	pub fn save(&mut self) -> std::io::Result<()> {
		let text = self.text();
		std::fs::write(&self.path, text)?;
		self.modified = false;
		self.status = String::new();
		Ok(())
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
	}

	pub fn undo(&mut self) {
		if let Some((lines, cursor)) = self.undo.pop() {
			self.redo.push((std::mem::replace(&mut self.lines, lines), self.cursor));
			self.cursor = cursor;
			self.anchor = None;
			self.column_goal = None;
			self.modified = true;
			self.hl_dirty = true;
			self.highlight();
		}
	}

	pub fn redo(&mut self) {
		if let Some((lines, cursor)) = self.redo.pop() {
			self.undo.push((std::mem::replace(&mut self.lines, lines), self.cursor));
			self.cursor = cursor;
			self.anchor = None;
			self.column_goal = None;
			self.modified = true;
			self.hl_dirty = true;
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

	pub fn selected_text(&self) -> String {
		let Some((a, b)) = self.selection() else { return String::new() };
		let mut out = String::new();
		for row in a.0..=b.0 {
			let line = &self.lines[row];
			let from = if row == a.0 { a.1.min(line.len()) } else { 0 };
			let to = if row == b.0 { b.1.min(line.len()) } else { line.len() };
			out.extend(&line[from..to]);
			if row != b.0 {
				out.push('\n');
			}
		}
		out
	}

	pub fn select_all(&mut self) {
		self.anchor = Some((0, 0));
		let last = self.lines.len() - 1;
		self.cursor = (last, self.lines[last].len());
	}

	pub fn copy(&self) -> String {
		self.selected_text()
	}

	pub fn cut(&mut self) -> String {
		let text = self.selected_text();
		if !text.is_empty() {
			self.delete_selection();
		}
		text
	}

	pub fn paste(&mut self, text: &str) {
		if text.is_empty() {
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
	}

	fn delete_selection_inner(&mut self) -> bool {
		let Some((a, b)) = self.selection() else { return false };
		let (mut ar, mut ac) = a;
		let (br, mut bc) = b;
		bc = bc.min(self.lines[br].len());
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
	}

	fn insert_char_raw(&mut self, c: char) {
		let (row, col) = self.cursor;
		self.lines[row].insert(col, c);
		self.cursor.1 += 1;
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
		} else {
			self.insert_char_raw(c);
		}
		self.modified = true;
		self.hl_dirty = true;
	}

	pub fn split_line(&mut self) {
		if self.read_only {
			return;
		}
		self.push_undo();
		self.delete_selection_inner();
		let (row, col) = self.cursor;
		let tail: Vec<char> = self.lines[row][col..].to_vec();
		self.lines[row].truncate(col);
		self.lines.insert(row + 1, tail);
		self.cursor = (row + 1, 0);
		self.modified = true;
		self.hl_dirty = true;
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
			self.push_undo();
			self.lines[row].remove(col - 1);
			self.cursor.1 -= 1;
			self.modified = true;
			self.hl_dirty = true;
		} else if row > 0 {
			self.push_undo();
			let prev_len = self.lines[row - 1].len();
			let line = self.lines.remove(row);
			self.lines[row - 1].extend(line);
			self.cursor = (row - 1, prev_len);
			self.modified = true;
			self.hl_dirty = true;
		}
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
			self.lines[row].remove(col);
			self.modified = true;
			self.hl_dirty = true;
		} else if row + 1 < self.lines.len() {
			self.push_undo();
			let next = self.lines.remove(row + 1);
			self.lines[row].extend(next);
			self.modified = true;
			self.hl_dirty = true;
		}
	}

	pub fn move_cursor(&mut self, dr: i32, dc: i32, extend: bool) {
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
	}

	pub fn home(&mut self, extend: bool) {
		if extend && self.anchor.is_none() {
			self.anchor = Some(self.cursor);
		} else if !extend {
			self.anchor = None;
		}
		self.cursor.1 = 0;
		self.column_goal = None;
	}

	pub fn end(&mut self, extend: bool) {
		if extend && self.anchor.is_none() {
			self.anchor = Some(self.cursor);
		} else if !extend {
			self.anchor = None;
		}
		self.cursor.1 = self.lines[self.cursor.0].len();
		self.column_goal = None;
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
	}

	pub fn header(&self) -> String {
		format!("{} ({})", self.title(), self.lang.name())
	}

	pub fn highlight(&mut self) {
		if !self.hl_dirty {
			return;
		}
		self.hl_dirty = false;
		let lines = self.lines.len();
		self.hl = (0..lines).map(|i| vec![Hl::Plain; self.lines[i].len()]).collect();
		let Some(language) = self.lang.language() else { return };
		let mut parser = Parser::new();
		if parser.set_language(&language).is_err() {
			return;
		}
		let src = self.text();
		let bytes = src.as_bytes();
		let Some(tree) = parser.parse(&src, None) else { return };
		let mut starts = Vec::with_capacity(lines);
		let mut pos = 0usize;
		for line in &self.lines {
			starts.push(pos);
			pos += line.iter().map(|c| c.len_utf8()).sum::<usize>() + 1;
		}
		let linecol = |offset: usize| -> (usize, usize) {
			let idx = starts.partition_point(|&s| s <= offset).saturating_sub(1);
			let rel = offset.saturating_sub(starts.get(idx).copied().unwrap_or(0));
			let text = &self.lines.get(idx).map(|l| l.iter().collect::<String>()).unwrap_or_default();
			let col = text.char_indices().take_while(|(b, _)| *b < rel).count();
			(idx, col)
		};
		let mut stack = vec![tree.root_node()];
		while let Some(node) = stack.pop() {
			let class = classify(node, bytes);
			let (sr, sc) = linecol(node.start_byte());
			let (er, ec) = linecol(node.end_byte());
			if class != Hl::Plain {
				for r in sr..=er.min(lines - 1) {
					let line_len = self.hl[r].len();
					let from = if r == sr { sc } else { 0 };
					let to = if r == er { (ec + 1).min(line_len) } else { line_len };
					for c in from..to {
						self.hl[r][c] = class;
					}
				}
			}
			let mut cursor = node.walk();
			for child in node.children(&mut cursor) {
				stack.push(child);
			}
		}
	}

	pub fn render(&mut self, canvas: &Canvas, area: Rect, focused: bool, show_cursor: bool) {
		self.highlight();
		let cw = canvas.cell_w;
		let ch = canvas.cell_h;
		let total_rows = (area.height() / ch).max(1) as usize;
		// A save error has nowhere else to show; the editor keeps it on its own
		// bottom row until the next successful save.
		let footer = if self.status.is_empty() { 0 } else { 1 };
		let rows = (total_rows - footer).max(1);
		let cols = (area.width() / cw).max(1) as usize;
		let bg = canvas::rgb(canvas::DEFAULT_BG.0, canvas::DEFAULT_BG.1, canvas::DEFAULT_BG.2);
		canvas.fill(area.left, area.top, area.left + cols as i32 * cw, area.top + total_rows as i32 * ch, bg);
		let saved = canvas.clip((area.left, area.top, area.left + cols as i32 * cw, area.top + total_rows as i32 * ch));

		if self.cursor.0 < self.scroll {
			self.scroll = self.cursor.0;
		}
		if self.cursor.0 >= self.scroll + rows {
			self.scroll = self.cursor.0 + 1 - rows;
		}
		let sel = self.selection();
		let mut visual_col = 0usize;
		for (i, c) in self.lines[self.cursor.0].iter().take(self.cursor.1).enumerate() {
			let _ = i;
			visual_col += if *c == '\t' { 4 - visual_col % 4 } else { char_cells(*c) };
		}
		if visual_col < self.hscroll {
			self.hscroll = visual_col;
		}
		if visual_col >= self.hscroll + cols {
			self.hscroll = visual_col + 1 - cols;
		}

		let fg_plain = canvas::rgb(canvas::DEFAULT_FG.0, canvas::DEFAULT_FG.1, canvas::DEFAULT_FG.2);
		for row in 0..rows {
			let line_idx = self.scroll + row;
			if line_idx >= self.lines.len() {
				break;
			}
			let y = area.top + row as i32 * ch;
			let line = &self.lines[line_idx];
			let mut col = 0usize;
			let mut x_cells = 0usize;
			for (ci, c) in line.iter().enumerate() {
				if *c == '\t' {
					let next = 4 - x_cells % 4;
					x_cells += next;
					col = ci;
					continue;
				}
				if x_cells >= self.hscroll {
					let screen_col = x_cells - self.hscroll;
					if screen_col >= cols {
						break;
					}
					let width = char_cells(*c);
					let mut fg = fg_plain;
					let class = self.hl.get(line_idx).and_then(|l| l.get(ci)).copied().unwrap_or(Hl::Plain);
					fg = hl_color(class, fg_plain);
					let selected = sel.is_some_and(|(a, b)| (line_idx, ci) >= a && (line_idx, ci) < b);
					if selected {
						canvas.fill(
							area.left + screen_col as i32 * cw,
							y,
							area.left + (screen_col as i32 + width as i32) * cw,
							y + ch,
							canvas::rgb(canvas::SELECTION_BG.0, canvas::SELECTION_BG.1, canvas::SELECTION_BG.2),
						);
					}
					let is_cursor = focused && show_cursor && self.cursor == (line_idx, ci);
					if is_cursor {
						canvas.fill(
							area.left + screen_col as i32 * cw,
							y,
							area.left + (screen_col as i32 + width as i32) * cw,
							y + ch,
							canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
						);
						fg = canvas::rgb(10, 12, 16);
					}
					let mut buf = [0u8; 4];
					let s = c.encode_utf8(&mut buf);
					canvas.text(area.left + screen_col as i32 * cw, y, s, fg);
				}
				x_cells += char_cells(*c);
				col = ci;
			}
			let _ = col;
			if focused && show_cursor && self.cursor.0 == line_idx && self.cursor.1 >= line.len() {
				let screen_col = x_cells.saturating_sub(self.hscroll);
				if screen_col < cols {
					canvas.fill(
						area.left + screen_col as i32 * cw,
						y,
						area.left + (screen_col as i32 + 1) * cw,
						y + ch,
						canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2),
					);
				}
			}
		}
		if footer == 1 {
			let y = area.top + (rows as i32) * ch;
			canvas.fill(
				area.left,
				y,
				area.left + cols as i32 * cw,
				y + ch,
				canvas::rgb(70, 20, 24),
			);
			let text: String = self.status.chars().take(cols).collect();
			canvas.text(area.left, y, &text, canvas::rgb(255, 200, 200));
		}
		canvas.unclip(saved);
	}
}

/// Screen cells one character occupies; tabs are handled by the caller.
pub fn char_cells(c: char) -> usize {
	c.width().unwrap_or(1).max(1)
}

fn hl_color(class: Hl, plain: windows_sys::Win32::Foundation::COLORREF) -> windows_sys::Win32::Foundation::COLORREF {	match class {
		Hl::Plain => plain,
		Hl::Keyword => canvas::rgb(198, 146, 220),
		Hl::String => canvas::rgb(152, 195, 121),
		Hl::Comment => canvas::rgb(110, 118, 129),
		Hl::Number => canvas::rgb(215, 178, 120),
		Hl::Function => canvas::rgb(120, 180, 235),
		Hl::Type => canvas::rgb(120, 205, 190),
		Hl::Constant => canvas::rgb(220, 160, 120),
		Hl::Property => canvas::rgb(200, 190, 160),
	}
}

fn classify(node: tree_sitter::Node, source: &[u8]) -> Hl {
	let kind = node.kind();
	if kind.contains("comment") {
		return Hl::Comment;
	}
	if kind.contains("string") || kind.contains("char_literal") || kind.contains("raw_string") {
		return Hl::String;
	}
	if kind.contains("integer") || kind.contains("float") || kind.contains("number") {
		return Hl::Number;
	}
	if kind == "escape_sequence" {
		return Hl::Constant;
	}
	if !node.is_named() {
		let text = node.utf8_text(source).unwrap_or("");
		if is_keyword(text) {
			return Hl::Keyword;
		}
		return Hl::Plain;
	}
	if kind == "type_identifier" || kind == "primitive_type" || kind == "typename" {
		return Hl::Type;
	}
	if kind == "field_identifier" || kind == "property_identifier" {
		return Hl::Property;
	}
	if kind == "identifier" || kind == "constant" {
		let parent = node.parent().map(|p| p.kind()).unwrap_or("");
		if parent == "call_expression" || parent == "function_item" || parent == "function_definition" {
			if let Some(parent) = node.parent() {
				if parent.child_by_field_name("function").map(|f| f.id() == node.id()).unwrap_or(false)
					|| parent.child_by_field_name("name").map(|f| f.id() == node.id()).unwrap_or(false)
				{
					return Hl::Function;
				}
			}
		}
		if node.utf8_text(source).map(|t| t.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())).unwrap_or(false) {
			return Hl::Constant;
		}
	}
	Hl::Plain
}

fn is_keyword(text: &str) -> bool {
	matches!(
		text,
		"fn" | "let" | "mut" | "pub" | "use" | "mod" | "impl" | "struct" | "enum" | "trait"
			| "for" | "while" | "loop" | "if" | "else" | "match" | "return" | "break" | "continue"
			| "const" | "static" | "unsafe" | "where" | "as" | "in" | "ref" | "move" | "async"
			| "await" | "dyn" | "crate" | "self" | "Self" | "super" | "def" | "class" | "import"
			| "from" | "lambda" | "pass" | "raise" | "try" | "except" | "finally" | "with" | "yield"
			| "global" | "nonlocal" | "assert" | "del" | "not" | "and" | "or" | "is" | "None"
			| "True" | "False" | "function" | "var" | "new" | "this" | "typeof" | "instanceof"
			| "void" | "delete" | "throw" | "catch" | "switch" | "case" | "default" | "extends"
			| "static" | "public" | "private" | "protected" | "interface" | "type" | "namespace"
			| "template" | "typename" | "virtual" | "operator" | "friend" | "using" | "nullptr"
			| "true" | "false" | "null" | "undefined" | "int" | "char" | "float" | "double"
			| "long" | "short" | "signed" | "unsigned" | "bool" | "wchar_t" | "auto" | "extern"
			| "register" | "volatile" | "sizeof" | "goto" | "do" | "export" | "package"
	)
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
}
