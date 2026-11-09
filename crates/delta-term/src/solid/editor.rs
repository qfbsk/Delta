use std::io;
use std::path::PathBuf;

use crate::keys::*;

use super::buffer::Buffer;
use super::highlight::{Highlight, Highlighter, Language};
use super::lsp::{CompletionItem, Diagnostic, LspClient, LspEvent};

pub struct Editor {
	pub path: PathBuf,
	pub buffer: Buffer,
	pub dirty: bool,
	pub language: Language,
	pub marks: Highlight,
	pub scroll: usize,
	pub diagnostics: Vec<Diagnostic>,
	pub completions: Vec<CompletionItem>,
	pub completion_index: usize,
	pub completion_open: bool,
	pub completion_line: usize,
	pub completion_col: usize,
	pub status: String,
	highlighter: Highlighter,
	lsp: Option<LspClient>,
	pending_completion: Option<i64>,
	completion_start: usize,
}

impl Editor {
	pub fn open(path: PathBuf, hwnd: usize) -> io::Result<Self> {
		let bytes = std::fs::read(&path)?;
		let source = String::from_utf8_lossy(&bytes).into_owned();
		let buffer = Buffer::from_text(&source);
		let language = Language::from_path(&path);
		let mut highlighter = Highlighter::new(language);
		let marks = highlighter.compute(&buffer.text());
		let mut editor = Self {
			path,
			buffer,
			dirty: false,
			language,
			marks,
			scroll: 0,
			diagnostics: Vec::new(),
			completions: Vec::new(),
			completion_index: 0,
			completion_open: false,
			completion_line: 0,
			completion_col: 0,
			status: String::new(),
			highlighter,
			lsp: None,
			pending_completion: None,
			completion_start: 0,
		};
		let text = editor.buffer.text();
		match LspClient::start(&editor.path, language, &text, hwnd) {
			Ok(client) => {
				editor.lsp = Some(client);
				editor.status = format!("{} lsp", language.name());
			}
			Err(err) => {
				editor.status = format!("{}: {err}", language.name());
			}
		}
		Ok(editor)
	}

	pub fn title(&self) -> String {
		self.path
			.file_name()
			.map(|name| name.to_string_lossy().into_owned())
			.unwrap_or_else(|| self.path.to_string_lossy().into_owned())
	}

	pub fn line_count(&self) -> usize {
		self.buffer.line_count()
	}

	pub fn error_count(&self) -> usize {
		self.diagnostics.iter().filter(|item| item.severity <= 2).count()
	}

	pub fn insert_str(&mut self, text: &str) {
		self.buffer.insert_str(text);
		self.changed();
	}

	pub fn save(&mut self) -> Result<(), String> {
		let text = self.buffer.text();
		std::fs::write(&self.path, text).map_err(|err| err.to_string())?;
		self.dirty = false;
		if let Some(lsp) = self.lsp.as_ref() {
			lsp.did_save();
		}
		self.status = String::from("saved");
		Ok(())
	}

	pub fn poll(&mut self) -> bool {
		let mut changed = false;
		loop {
			let event = match self.lsp.as_ref().and_then(LspClient::try_event) {
				Some(event) => event,
				None => break,
			};
			match event {
				LspEvent::Diagnostics(items) => {
					self.diagnostics = items;
					changed = true;
				}
				LspEvent::Completions { id, items } => {
					if self.pending_completion == Some(id) {
						self.pending_completion = None;
						if !items.is_empty() {
							self.completions = items;
							self.completion_index = 0;
							self.completion_open = true;
						}
						changed = true;
					}
				}
				LspEvent::Status(status) => {
					self.status = status;
					changed = true;
				}
			}
		}
		changed
	}

	pub fn on_key(&mut self, vk: u32, ctrl: bool, _shift: bool, page_rows: usize) -> bool {
		if ctrl && vk == VK_S {
			if let Err(err) = self.save() {
				self.status = err;
			}
			return true;
		}
		if ctrl && vk == VK_SPACE {
			self.request_completion();
			return true;
		}
		if self.completion_open {
			match vk {
				VK_UP => {
					if self.completion_index > 0 {
						self.completion_index -= 1;
					} else {
						self.completion_index = self.completions.len().saturating_sub(1);
					}
					return true;
				}
				VK_DOWN => {
					if !self.completions.is_empty() {
						self.completion_index = (self.completion_index + 1) % self.completions.len();
					}
					return true;
				}
				VK_RETURN | VK_TAB => {
					self.apply_completion();
					return true;
				}
				VK_ESCAPE => {
					self.completion_open = false;
					return true;
				}
				_ => {}
			}
		}
		match vk {
			VK_LEFT => self.buffer.move_left(),
			VK_RIGHT => self.buffer.move_right(),
			VK_UP => self.buffer.move_up(),
			VK_DOWN => self.buffer.move_down(),
			VK_HOME => {
				if ctrl {
					self.buffer.line = 0;
				}
				self.buffer.home();
			}
			VK_END => {
				if ctrl {
					self.buffer.line = self.buffer.lines.len() - 1;
				}
				self.buffer.end();
			}
			VK_PRIOR => self.scroll = self.scroll.saturating_sub(page_rows.max(1)),
			VK_NEXT => {
				let max = self.buffer.line_count().saturating_sub(1);
				self.scroll = (self.scroll + page_rows.max(1)).min(max);
			}
			VK_BACK => {
				if self.buffer.backspace() {
					self.changed();
				}
			}
			VK_DELETE => {
				if self.buffer.delete() {
					self.changed();
				}
			}
			VK_RETURN => {
				self.buffer.newline();
				self.changed();
			}
			VK_TAB => {
				self.buffer.insert_char('\t');
				self.changed();
			}
			_ => return false,
		}
		self.completion_open = false;
		self.ensure_visible(page_rows);
		true
	}

	pub fn insert_char(&mut self, ch: char) {
		if ch == '\r' || ch == '\n' || ch == '\t' || ch == '\u{8}' {
			return;
		}
		if (ch as u32) < 0x20 || ch == '\u{7f}' {
			return;
		}
		self.buffer.insert_char(ch);
		self.changed();
	}

	pub fn click(&mut self, text_col: usize, row: usize, page_rows: usize) {
		let line = (self.scroll + row).min(self.buffer.line_count().saturating_sub(1));
		self.buffer.set_cursor(line, text_col);
		self.completion_open = false;
		self.ensure_visible(page_rows);
	}

	pub fn ensure_visible(&mut self, rows: usize) {
		let rows = rows.max(1);
		if self.buffer.line < self.scroll {
			self.scroll = self.buffer.line;
		} else if self.buffer.line >= self.scroll + rows {
			self.scroll = self.buffer.line + 1 - rows;
		}
		let max = self.buffer.line_count().saturating_sub(1);
		self.scroll = self.scroll.min(max);
	}

	pub fn diagnostic_at(&self, line: usize, col: usize) -> Option<u8> {
		for item in &self.diagnostics {
			if item.line == line && col >= item.start && col < item.end {
				return Some(item.severity);
			}
		}
		None
	}

	fn changed(&mut self) {
		self.dirty = true;
		self.completion_open = false;
		let text = self.buffer.text();
		self.marks = self.highlighter.compute(&text);
		if let Some(lsp) = self.lsp.as_mut() {
			lsp.did_change(&text);
		}
	}

	fn request_completion(&mut self) {
		if self.buffer.line >= self.buffer.line_count() {
			return;
		}
		let (start, prefix) = self.buffer.word_prefix();
		let mut items: Vec<CompletionItem> = self
			.buffer
			.identifiers()
			.into_iter()
			.filter(|word| word.starts_with(&prefix) && *word != prefix)
			.take(20)
			.map(|word| CompletionItem {
				label: word.clone(),
				insert: word,
				detail: String::from("buffer"),
				kind: 0,
			})
			.collect();
		items.sort_by(|a, b| a.label.cmp(&b.label));
		self.completion_start = start;
		self.completion_line = self.buffer.line;
		self.completion_col = self.buffer.col;
		if let Some(lsp) = self.lsp.as_mut() {
			let id = lsp.request_completion(self.buffer.line, self.buffer.col);
			self.pending_completion = Some(id);
		}
		self.completions = items;
		self.completion_index = 0;
		self.completion_open = !self.completions.is_empty();
	}

	fn apply_completion(&mut self) {
		if let Some(item) = self.completions.get(self.completion_index) {
			let text = item.insert.clone();
			self.buffer.replace_prefix(self.completion_start, &text);
			self.changed();
		}
		self.completion_open = false;
	}
}
