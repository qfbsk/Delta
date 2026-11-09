use std::collections::HashMap;
use std::io::Cursor;

use base64::Engine as _;

use crate::term::{Attrs, Color, ImagePlacement, Terminal};

#[derive(Clone, Copy, PartialEq)]
enum State {
	Ground,
	Escape,
	Charset,
	Csi,
	Osc,
	OscEscape,
	Dcs,
	DcsEscape,
	Apc,
	ApcEscape,
}

pub struct Parser {
	state: State,
	params: Vec<u16>,
	current: Option<u16>,
	private: u8,
	intermediate: u8,
	utf8_need: usize,
	utf8_len: usize,
	utf8_code: u32,
	apc: Vec<u8>,
}

impl Parser {
	pub fn new() -> Self {
		Self {
			state: State::Ground,
			params: Vec::new(),
			current: None,
			private: 0,
			intermediate: 0,
			utf8_need: 0,
			utf8_len: 0,
			utf8_code: 0,
			apc: Vec::new(),
		}
	}

	pub fn feed(&mut self, bytes: &[u8], term: &mut Terminal, responses: &mut Vec<u8>) {
		for &b in bytes {
			self.byte(b, term, responses);
		}
	}

	fn byte(&mut self, b: u8, term: &mut Terminal, responses: &mut Vec<u8>) {
		match self.state {
			State::Ground => self.ground(b, term),
			State::Escape => self.escape(b, term),
			State::Charset => {
				self.state = State::Ground;
			}
			State::Csi => self.csi(b, term, responses),
			State::Osc => {
				if b == 0x07 {
					self.state = State::Ground;
				} else if b == 0x1b {
					self.state = State::OscEscape;
				} else if b == 0x18 || b == 0x1a {
					self.state = State::Ground;
				}
			}
			State::OscEscape => {
				if b == b'\\' {
					self.state = State::Ground;
				} else {
					self.state = State::Osc;
				}
			}
			State::Dcs => {
				if b == 0x1b {
					self.state = State::DcsEscape;
				} else if b == 0x18 || b == 0x1a {
					self.state = State::Ground;
				}
			}
			State::DcsEscape => {
				if b == b'\\' {
					self.state = State::Ground;
				} else {
					self.state = State::Dcs;
				}
			}
			State::Apc => {
				if b == 0x1b {
					self.state = State::ApcEscape;
				} else {
					self.apc.push(b);
				}
			}
			State::ApcEscape => {
				if b == b'\\' {
					self.finish_apc(term, responses);
					self.state = State::Ground;
				} else {
					self.apc.push(0x1b);
					self.apc.push(b);
					self.state = State::Apc;
				}
			}
		}
	}

	fn ground(&mut self, b: u8, term: &mut Terminal) {
		if b < 0x20 {
			match b {
				0x07 => {}
				0x08 => term.active_mut().backspace(),
				0x09 => term.active_mut().tab(),
				0x0a | 0x0b | 0x0c => term.active_mut().newline(),
				0x0d => term.active_mut().carriage_return(),
				0x1b => self.state = State::Escape,
				_ => {}
			}
			return;
		}
		if b == 0x7f {
			return;
		}
		if b < 0x80 {
			term.put_char(b as char);
			return;
		}
		if let Some(ch) = self.utf8(b) {
			term.put_char(ch);
		}
	}

	fn escape(&mut self, b: u8, term: &mut Terminal) {
		self.state = State::Ground;
		match b {
			b'[' => {
				self.params.clear();
				self.current = None;
				self.private = 0;
				self.intermediate = 0;
				self.state = State::Csi;
			}
			b']' => self.state = State::Osc,
			b'P' => self.state = State::Dcs,
			b'_' => {
				self.apc.clear();
				self.state = State::Apc;
			}
			b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => self.state = State::Charset,
			b'7' => term.active_mut().save_cursor(),
			b'8' => term.active_mut().restore_cursor(),
			b'D' => term.active_mut().newline(),
			b'E' => {
				term.active_mut().newline();
				term.active_mut().carriage_return();
			}
			b'M' => {
				let s = term.active_mut();
				if s.cursor_y == s.scroll_top {
					s.scroll_down(1);
				} else {
					s.cursor_y = s.cursor_y.saturating_sub(1);
				}
				s.wrap_pending = false;
			}
			b'c' => term.reset(),
			_ => {}
		}
	}

	fn csi(&mut self, b: u8, term: &mut Terminal, responses: &mut Vec<u8>) {
		if b == 0x1b {
			self.state = State::Escape;
			return;
		}
		if b == 0x18 || b == 0x1a {
			self.state = State::Ground;
			return;
		}
		if b < 0x20 {
			match b {
				0x08 => term.active_mut().backspace(),
				0x09 => term.active_mut().tab(),
				0x0a | 0x0b | 0x0c => term.active_mut().newline(),
				0x0d => term.active_mut().carriage_return(),
				_ => {}
			}
			return;
		}
		if b == b'?' && self.params.is_empty() && self.current.is_none() {
			self.private = b'?';
			return;
		}
		if (b == b'>' || b == b'=') && self.params.is_empty() && self.current.is_none() {
			self.private = b;
			return;
		}
		if b.is_ascii_digit() {
			let digit = (b - b'0') as u32;
			let cur = self.current.unwrap_or(0) as u32;
			self.current =
				Some((cur.saturating_mul(10).saturating_add(digit)).min(u16::MAX as u32) as u16);
			return;
		}
		if b == b';' || b == b':' {
			self.params.push(self.current.take().unwrap_or(0));
			return;
		}
		if (0x20..=0x2f).contains(&b) {
			self.intermediate = b;
			return;
		}
		if (0x40..=0x7e).contains(&b) {
			if let Some(cur) = self.current.take() {
				self.params.push(cur);
			}
			self.state = State::Ground;
			self.dispatch(b, term, responses);
		}
	}

	fn dispatch(&mut self, final_byte: u8, term: &mut Terminal, responses: &mut Vec<u8>) {
		let params = self.params.clone();
		let p = |i: usize, default: usize| -> usize {
			params
				.get(i)
				.map(|&v| if v == 0 { default } else { v as usize })
				.unwrap_or(default)
		};
		let raw = |i: usize| -> usize { params.get(i).copied().unwrap_or(0) as usize };
		match final_byte {
			b'@' => term.active_mut().insert_chars(p(0, 1)),
			b'A' => {
				let s = term.active_mut();
				s.cursor_y = s.cursor_y.saturating_sub(p(0, 1));
				s.wrap_pending = false;
			}
			b'B' => {
				let s = term.active_mut();
				s.cursor_y = (s.cursor_y + p(0, 1)).min(s.rows - 1);
				s.wrap_pending = false;
			}
			b'C' => {
				let s = term.active_mut();
				s.cursor_x = (s.cursor_x + p(0, 1)).min(s.cols - 1);
				s.wrap_pending = false;
			}
			b'D' => {
				let s = term.active_mut();
				s.cursor_x = s.cursor_x.saturating_sub(p(0, 1));
				s.wrap_pending = false;
			}
			b'E' => {
				for _ in 0..p(0, 1) {
					term.active_mut().newline();
				}
				term.active_mut().carriage_return();
			}
			b'F' => {
				let s = term.active_mut();
				s.cursor_y = s.cursor_y.saturating_sub(p(0, 1));
				s.cursor_x = 0;
				s.wrap_pending = false;
			}
			b'G' => {
				let s = term.active_mut();
				s.cursor_x = (p(0, 1) - 1).min(s.cols - 1);
				s.wrap_pending = false;
			}
			b'H' | b'f' => {
				let row = p(0, 1) - 1;
				let col = p(1, 1) - 1;
				term.active_mut().move_to(col, row);
			}
			b'J' => term.active_mut().erase_display(raw(0)),
			b'K' => term.active_mut().erase_line(raw(0)),
			b'L' => term.active_mut().insert_lines(p(0, 1)),
			b'M' => term.active_mut().delete_lines(p(0, 1)),
			b'P' => term.active_mut().delete_chars(p(0, 1)),
			b'S' => term.active_mut().scroll_up(p(0, 1)),
			b'T' => term.active_mut().scroll_down(p(0, 1)),
			b'X' => term.active_mut().erase_chars(p(0, 1)),
			b'd' => {
				let s = term.active_mut();
				s.cursor_y = (p(0, 1) - 1).min(s.rows - 1);
				s.wrap_pending = false;
			}
			b'm' => self.sgr(term),
			b'r' => {
				let rows = term.rows();
				let top = p(0, 1) - 1;
				let bottom = p(1, rows) - 1;
				if top < bottom && bottom < rows {
					let s = term.active_mut();
					s.scroll_top = top;
					s.scroll_bottom = bottom;
					s.move_to(0, top);
				}
			}
			b's' => term.active_mut().save_cursor(),
			b'u' => term.active_mut().restore_cursor(),
			b'h' => self.modes(true, term),
			b'l' => self.modes(false, term),
			b'n' => {
				if raw(0) == 6 {
					let s = term.active();
					let seq = format!("\x1b[{};{}R", s.cursor_y + 1, s.cursor_x + 1);
					responses.extend_from_slice(seq.as_bytes());
				} else if raw(0) == 5 {
					responses.extend_from_slice(b"\x1b[0n");
				}
			}
			b'c' => {
				if self.private == b'>' {
					responses.extend_from_slice(b"\x1b[>0;1;0c");
				} else {
					responses.extend_from_slice(b"\x1b[?1;2;4c");
				}
			}
			_ => {}
		}
	}

	fn modes(&mut self, on: bool, term: &mut Terminal) {
		if self.private == b'?' {
			for &p in &self.params {
				match p {
					1 => term.app_cursor = on,
					7 => term.wrap_mode = on,
					25 => term.cursor_visible = on,
					47 | 1047 | 1049 => {
						if on {
							term.enter_alt();
						} else {
							term.leave_alt();
						}
					}
					_ => {}
				}
			}
		}
	}

	fn finish_apc(&mut self, term: &mut Terminal, responses: &mut Vec<u8>) {
		if self.apc.first() == Some(&b'G') {
			let _ = self.kitty(term, responses);
		}
		self.apc.clear();
	}

	fn kitty(&mut self, term: &mut Terminal, responses: &mut Vec<u8>) -> Result<(), String> {
		let data = &self.apc[1..];
		let (keys, payload) = match data.iter().position(|&b| b == b';') {
			Some(pos) => (&data[..pos], &data[pos + 1..]),
			None => (data, &[][..]),
		};
		let keys = std::str::from_utf8(keys).map_err(|_| String::from("apc keys"))?;
		let mut map = HashMap::new();
		for part in keys.split(',') {
			if part.is_empty() {
				continue;
			}
			if let Some((key, value)) = part.split_once('=') {
				if let Some(key) = key.chars().next() {
					map.insert(key, value.to_string());
				}
			} else if let Some(key) = part.chars().next() {
				map.insert(key, String::new());
			}
		}
		let action = map.get(&'a').map(String::as_str).unwrap_or("t");
		let id = map.get(&'i').and_then(|value| value.parse::<u32>().ok());
		if action == "d" {
			term.delete_images(id);
			return Ok(());
		}
		if action == "q" {
			if let Some(id) = id {
				let response = format!("_Gi={id};OK\\");
				responses.extend_from_slice(response.as_bytes());
			}
			return Ok(());
		}
		if action == "p" {
			if let Some(id) = id {
				let screen = term.active();
				let cols = map.get(&'c').and_then(|value| value.parse().ok());
				let rows = map.get(&'r').and_then(|value| value.parse().ok());
				let x = screen.cursor_x;
				let y = screen.cursor_y;
				term.place_image(id, x, y, cols, rows);
			}
			return Ok(());
		}
		if map.get(&'t').map(String::as_str).unwrap_or("d") != "d" {
			return Ok(());
		}
		if map.get(&'m').map(String::as_str) == Some("1") {
			return Ok(());
		}
		let bytes = base64::engine::general_purpose::STANDARD
			.decode(payload)
			.map_err(|err| err.to_string())?;
		let format = map.get(&'f').map(String::as_str).unwrap_or("32");
		let (width, height, rgba) = match format {
			"100" => decode_png(&bytes)?,
			"24" => {
				let width = map
					.get(&'s')
					.and_then(|value| value.parse::<usize>().ok())
					.ok_or_else(|| String::from("missing width"))?;
				let height = map
					.get(&'v')
					.and_then(|value| value.parse::<usize>().ok())
					.ok_or_else(|| String::from("missing height"))?;
				if bytes.len() < width * height * 3 {
					return Err(String::from("short rgb data"));
				}
				let mut rgba = Vec::with_capacity(width * height * 4);
				for chunk in bytes.chunks_exact(3) {
					rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
				}
				(width, height, rgba)
			}
			_ => {
				let width = map
					.get(&'s')
					.and_then(|value| value.parse::<usize>().ok())
					.ok_or_else(|| String::from("missing width"))?;
				let height = map
					.get(&'v')
					.and_then(|value| value.parse::<usize>().ok())
					.ok_or_else(|| String::from("missing height"))?;
				if bytes.len() < width * height * 4 {
					return Err(String::from("short rgba data"));
				}
				(width, height, bytes[..width * height * 4].to_vec())
			}
		};
		let placement = if matches!(action, "T" | "p") {
			let screen = term.active();
			Some(ImagePlacement {
				x: screen.cursor_x,
				y: screen.cursor_y,
				cols: map.get(&'c').and_then(|value| value.parse().ok()),
				rows: map.get(&'r').and_then(|value| value.parse().ok()),
			})
		} else {
			None
		};
		term.set_image(id, width, height, rgba, placement);
		Ok(())
	}

	fn sgr(&mut self, term: &mut Terminal) {
		let params = self.params.clone();
		let mut attrs = term.attrs;
		let mut i = 0;
		while i < params.len() {
			let p = params[i] as usize;
			match p {
				0 => attrs = Attrs::default(),
				1 => attrs.bold = true,
				4 => attrs.underline = true,
				7 => attrs.reverse = true,
				22 => attrs.bold = false,
				24 => attrs.underline = false,
				27 => attrs.reverse = false,
				30..=37 => attrs.fg = Color::Indexed((p - 30) as u8),
				39 => attrs.fg = Color::Default,
				40..=47 => attrs.bg = Color::Indexed((p - 40) as u8),
				49 => attrs.bg = Color::Default,
				90..=97 => attrs.fg = Color::Indexed((p - 90 + 8) as u8),
				100..=107 => attrs.bg = Color::Indexed((p - 100 + 8) as u8),
				38 | 48 => {
					let target_fg = p == 38;
					if i + 2 < params.len() && params[i + 1] == 5 {
						let color = Color::Indexed(params[i + 2] as u8);
						if target_fg {
							attrs.fg = color;
						} else {
							attrs.bg = color;
						}
						i += 2;
					} else if i + 4 < params.len() && params[i + 1] == 2 {
						let color = Color::Rgb(
							params[i + 2] as u8,
							params[i + 3] as u8,
							params[i + 4] as u8,
						);
						if target_fg {
							attrs.fg = color;
						} else {
							attrs.bg = color;
						}
						i += 4;
					}
				}
				_ => {}
			}
			i += 1;
		}
		term.attrs = attrs;
	}

	fn utf8(&mut self, b: u8) -> Option<char> {
		if self.utf8_need == 0 {
			if b < 0x80 {
				return Some(b as char);
			}
			if (0xc2..=0xdf).contains(&b) {
				self.utf8_need = 2;
				self.utf8_len = 1;
				self.utf8_code = (b & 0x1f) as u32;
				return None;
			}
			if (0xe0..=0xef).contains(&b) {
				self.utf8_need = 3;
				self.utf8_len = 1;
				self.utf8_code = (b & 0x0f) as u32;
				return None;
			}
			if (0xf0..=0xf4).contains(&b) {
				self.utf8_need = 4;
				self.utf8_len = 1;
				self.utf8_code = (b & 0x07) as u32;
				return None;
			}
			return Some('\u{FFFD}');
		}
		if b & 0xc0 != 0x80 {
			self.utf8_need = 0;
			self.utf8_len = 0;
			self.utf8_code = 0;
			return Some('\u{FFFD}');
		}
		self.utf8_code = (self.utf8_code << 6) | (b & 0x3f) as u32;
		self.utf8_len += 1;
		if self.utf8_len == self.utf8_need {
			let code = self.utf8_code;
			self.utf8_need = 0;
			self.utf8_len = 0;
			self.utf8_code = 0;
			return Some(char::from_u32(code).unwrap_or('\u{FFFD}'));
		}
		None
	}
}

pub fn load_png(path: &str) -> Result<(usize, usize, Vec<u8>), String> {
	let bytes = std::fs::read(path).map_err(|err| err.to_string())?;
	decode_png(&bytes)
}

fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
	let decoder = png::Decoder::new(Cursor::new(bytes));
	let mut reader = decoder.read_info().map_err(|err| err.to_string())?;
	let mut buffer = vec![0u8; reader.output_buffer_size()];
	let info = reader
		.next_frame(&mut buffer)
		.map_err(|err| err.to_string())?;
	let width = info.width as usize;
	let height = info.height as usize;
	let source = &buffer[..info.buffer_size()];
	let rgba = match info.color_type {
		png::ColorType::Rgba => source.to_vec(),
		png::ColorType::Rgb => {
			let mut rgba = Vec::with_capacity(width * height * 4);
			for chunk in source.chunks_exact(3) {
				rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
			}
			rgba
		}
		png::ColorType::Grayscale => {
			let mut rgba = Vec::with_capacity(width * height * 4);
			for &value in source {
				rgba.extend_from_slice(&[value, value, value, 255]);
			}
			rgba
		}
		png::ColorType::GrayscaleAlpha => {
			let mut rgba = Vec::with_capacity(width * height * 4);
			for chunk in source.chunks_exact(2) {
				rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
			}
			rgba
		}
		png::ColorType::Indexed => return Err(String::from("indexed png")),
	};
	Ok((width, height, rgba))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_kitty_rgba_image() {
		let mut term = Terminal::new(80, 24);
		let mut parser = Parser::new();
		let mut responses = Vec::new();
		let seq = b"\x1b_Ga=T,f=32,i=7,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\";
		parser.feed(seq, &mut term, &mut responses);
		assert_eq!(term.images.len(), 1);
		assert_eq!(term.images[0].id, 7);
		assert_eq!(term.images[0].width, 2);
		assert_eq!(term.images[0].height, 2);
		assert!(term.images[0].placement.is_some());
	}
}
