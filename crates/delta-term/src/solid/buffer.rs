#[derive(Clone, Debug)]
pub struct Buffer {
	pub lines: Vec<Vec<char>>,
	pub line: usize,
	pub col: usize,
}

impl Buffer {
	pub fn from_text(text: &str) -> Self {
		let text = text.replace("\r\n", "\n").replace('\r', "\n");
		let lines: Vec<Vec<char>> = text.split('\n').map(|line| line.chars().collect()).collect();
		Self {
			lines: if lines.is_empty() { vec![Vec::new()] } else { lines },
			line: 0,
			col: 0,
		}
	}

	pub fn text(&self) -> String {
		self.lines
			.iter()
			.map(|line| line.iter().collect::<String>())
			.collect::<Vec<_>>()
			.join("\n")
	}

	pub fn line_count(&self) -> usize {
		self.lines.len()
	}

	pub fn line_len(&self, line: usize) -> usize {
		self.lines.get(line).map(Vec::len).unwrap_or(0)
	}

	pub fn current_len(&self) -> usize {
		self.line_len(self.line)
	}

	pub fn clamp(&mut self) {
		if self.lines.is_empty() {
			self.lines.push(Vec::new());
		}
		self.line = self.line.min(self.lines.len() - 1);
		self.col = self.col.min(self.lines[self.line].len());
	}

	pub fn set_cursor(&mut self, line: usize, col: usize) {
		self.line = line.min(self.lines.len().saturating_sub(1));
		self.col = col.min(self.lines[self.line].len());
	}

	pub fn move_left(&mut self) {
		if self.col > 0 {
			self.col -= 1;
		} else if self.line > 0 {
			self.line -= 1;
			self.col = self.current_len();
		}
	}

	pub fn move_right(&mut self) {
		if self.col < self.current_len() {
			self.col += 1;
		} else if self.line + 1 < self.lines.len() {
			self.line += 1;
			self.col = 0;
		}
	}

	pub fn move_up(&mut self) {
		if self.line > 0 {
			self.line -= 1;
			self.col = self.col.min(self.current_len());
		}
	}

	pub fn move_down(&mut self) {
		if self.line + 1 < self.lines.len() {
			self.line += 1;
			self.col = self.col.min(self.current_len());
		}
	}

	pub fn home(&mut self) {
		self.col = 0;
	}

	pub fn end(&mut self) {
		self.col = self.current_len();
	}

	pub fn insert_char(&mut self, ch: char) {
		self.lines[self.line].insert(self.col, ch);
		self.col += 1;
	}

	pub fn insert_str(&mut self, text: &str) {
		for ch in text.chars() {
			if ch == '\r' {
				continue;
			}
			if ch == '\n' {
				self.newline();
			} else {
				self.insert_char(ch);
			}
		}
	}

	pub fn newline(&mut self) {
		let tail: Vec<char> = self.lines[self.line].split_off(self.col);
		self.lines.insert(self.line + 1, tail);
		self.line += 1;
		self.col = 0;
	}

	pub fn backspace(&mut self) -> bool {
		if self.col > 0 {
			self.lines[self.line].remove(self.col - 1);
			self.col -= 1;
			true
		} else if self.line > 0 {
			let current = self.lines.remove(self.line);
			self.line -= 1;
			self.col = self.current_len();
			self.lines[self.line].extend(current);
			true
		} else {
			false
		}
	}

	pub fn delete(&mut self) -> bool {
		if self.col < self.current_len() {
			self.lines[self.line].remove(self.col);
			true
		} else if self.line + 1 < self.lines.len() {
			let next = self.lines.remove(self.line + 1);
			self.lines[self.line].extend(next);
			true
		} else {
			false
		}
	}

	pub fn word_prefix(&self) -> (usize, String) {
		let line = &self.lines[self.line];
		let mut start = self.col;
		while start > 0 && is_word(line[start - 1]) {
			start -= 1;
		}
		(start, line[start..self.col].iter().collect())
	}

	pub fn replace_prefix(&mut self, start: usize, text: &str) {
		let at = self.col.min(self.current_len());
		let start = start.min(at);
		let line = &mut self.lines[self.line];
		line.drain(start..at);
		for (offset, ch) in text.chars().enumerate() {
			line.insert(start + offset, ch);
		}
		self.col = start + text.chars().count();
	}

	pub fn identifiers(&self) -> Vec<String> {
		let mut seen = std::collections::BTreeSet::new();
		for line in &self.lines {
			let mut word = String::new();
			for &ch in line {
				if is_word(ch) {
					word.push(ch);
				} else {
					if word.len() > 1 {
						seen.insert(std::mem::take(&mut word));
					}
					word.clear();
				}
			}
			if word.len() > 1 {
				seen.insert(word);
			}
		}
		seen.into_iter().collect()
	}
}

pub fn is_word(ch: char) -> bool {
	ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn edits_roundtrip() {
		let mut buffer = Buffer::from_text("fn main() {\n\tlet x = 1;\n}");
		buffer.set_cursor(1, 5);
		buffer.insert_str("mut ");
		buffer.set_cursor(2, 0);
		buffer.insert_char('/');
		assert_eq!(buffer.text(), "fn main() {\n\tlet mut x = 1;\n/}");
		buffer.backspace();
		assert_eq!(buffer.text(), "fn main() {\n\tlet mut x = 1;\n}");
	}

	#[test]
	fn joins_lines_on_backspace() {
		let mut buffer = Buffer::from_text("ab\ncd");
		buffer.set_cursor(1, 0);
		assert!(buffer.backspace());
		assert_eq!(buffer.text(), "abcd");
		assert_eq!(buffer.col, 2);
	}
}
