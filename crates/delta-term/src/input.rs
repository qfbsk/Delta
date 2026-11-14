//! Windows key encoding: legacy xterm, win32-input-mode 9001 and the kitty
//! keyboard protocol. Transcribed from wezterm 20240203
//! (`termwiz/src/input.rs::KeyCode::encode`,
//! `wezterm-input-types/src/lib.rs::encode_win32_input_mode/encode_kitty`)
//! and Windows Terminal 1.26 (`terminalInput.cpp::_makeWin32Output`).

use std::io::Write as _;

pub const MODE_SHIFT: u32 = 0x10;
pub const MODE_LEFT_ALT: u32 = 0x2;
pub const MODE_RIGHT_ALT: u32 = 0x1;
pub const MODE_LEFT_CTRL: u32 = 0x8;
pub const MODE_RIGHT_CTRL: u32 = 0x4;
pub const MODE_ENHANCED: u32 = 0x100;

#[derive(Clone, Copy, Debug, Default)]
pub struct Key {
	pub vk: u16,
	pub scan: u16,
	pub ch: Option<char>,
	pub ctrl: bool,
	pub alt: bool,
	pub shift: bool,
	pub right_alt: bool,
	pub right_ctrl: bool,
	pub enhanced: bool,
	pub down: bool,
	pub repeat: u16,
	pub caps: bool,
	pub num: bool,
	pub cursor_keys: bool,
}

impl Key {
	pub fn control_state(&self) -> u32 {
		let mut cs = 0;
		if self.shift {
			cs |= MODE_SHIFT;
		}
		if self.right_alt {
			cs |= MODE_RIGHT_ALT;
		} else if self.alt {
			cs |= MODE_LEFT_ALT;
		}
		if self.right_ctrl {
			cs |= MODE_RIGHT_CTRL;
		} else if self.ctrl {
			cs |= MODE_LEFT_CTRL;
		}
		if self.enhanced {
			cs |= MODE_ENHANCED;
		}
		if self.caps {
			cs |= 0x80;
		}
		if self.num {
			cs |= 0x20;
		}
		cs
	}

	fn xterm_mod(&self) -> u32 {
		let mut m = 0;
		if self.shift {
			m |= 1;
		}
		if self.alt {
			m |= 2;
		}
		if self.ctrl {
			m |= 4;
		}
		m + 1
	}

	fn any_mod(&self) -> bool {
		self.shift || self.alt || self.ctrl
	}
}

pub fn encode_win32(key: &Key) -> String {
	let uc = if key.down {
		key.ch.map(|c| c as u32).unwrap_or(0)
	} else {
		0
	};
	format!(
		"\x1b[{};{};{};{};{};{}_",
		key.vk,
		key.scan,
		uc,
		if key.down { 1 } else { 0 },
		key.control_state(),
		key.repeat.max(1)
	)
}

fn ctrl_byte(c: char) -> Option<u8> {
	match c {
		'@'..='_' => Some((c as u32 & 0x1f) as u8),
		'a'..='z' => Some((c as u32 & 0x1f) as u8),
		' ' => Some(0),
		'/' => Some(0x1f),
		'?' => Some(0x7f),
		'2'..='8' => Some([0, 27, 28, 29, 30, 31, 127][(c as u8 - b'2') as usize]),
		_ => None,
	}
}

pub fn encode_legacy(key: &Key) -> Vec<u8> {
	let mut out = Vec::new();
	if !key.down {
		return out;
	}
	let s = match key.vk {
		0x25 => Some("D"), // left
		0x26 => Some("A"), // up
		0x27 => Some("C"), // right
		0x28 => Some("B"), // down
		0x24 => Some("H"), // home
		0x23 => Some("F"), // end
		_ => None,
	};
	if let Some(final_byte) = s {
		if key.any_mod() {
			let _ = write!(out, "\x1b[1;{}{}", key.xterm_mod(), final_byte);
		} else if key.cursor_keys {
			let _ = write!(out, "\x1bO{final_byte}");
		} else {
			let _ = write!(out, "\x1b[{final_byte}");
		}
		return out;
	}
	let tilde = match key.vk {
		0x2D => Some(2),  // insert
		0x2E => Some(3),  // delete
		0x21 => Some(5),  // page up
		0x22 => Some(6),  // page down
		0x74 => Some(15), // F5
		0x75 => Some(17),
		0x76 => Some(18),
		0x77 => Some(19),
		0x78 => Some(20),
		0x79 => Some(21),
		0x7A => Some(23),
		0x7B => Some(24),
		_ => None,
	};
	if let Some(n) = tilde {
		if key.any_mod() {
			let _ = write!(out, "\x1b[{};{}~", n, key.xterm_mod());
		} else {
			let _ = write!(out, "\x1b[{n}~");
		}
		return out;
	}
	if matches!(key.vk, 0x70..=0x73) {
		let c = match key.vk {
			0x70 => "P",
			0x71 => "Q",
			0x72 => "R",
			0x73 => "S",
			_ => "",
		};
		if key.any_mod() {
			let _ = write!(out, "\x1b[1;{}{}", key.xterm_mod(), c);
		} else {
			let _ = write!(out, "\x1bO{c}");
		}
		return out;
	}
	match key.vk {
		0x0D => {
			if key.shift || key.ctrl {
				let _ = write!(out, "\x1b[13;{}u", key.xterm_mod());
			} else {
				if key.alt {
					out.push(0x1b);
				}
				out.push(b'\r');
			}
			return out;
		}
		0x1B => {
			if key.shift || key.ctrl || key.alt {
				let _ = write!(out, "\x1b[27;{}u", key.xterm_mod());
			} else {
				out.push(0x1b);
			}
			return out;
		}
		0x09 => {
			if key.shift {
				out.extend_from_slice(b"\x1b[Z");
			} else if key.ctrl {
				out.extend_from_slice(b"\x1b[9;5u");
			} else {
				out.push(b'\t');
			}
			return out;
		}
		0x08 => {
			if key.ctrl {
				out.push(0x08);
			} else {
				out.push(0x7f);
			}
			return out;
		}
		_ => {}
	}
	if key.vk == 0x20 && key.ctrl {
		out.push(0);
		return out;
	}
	let Some(ch) = key.ch else {
		return out;
	};
	if key.ctrl {
		if let Some(byte) = ctrl_byte(ch) {
			if key.alt {
				out.push(0x1b);
			}
			out.push(byte);
			return out;
		}
	}
	if key.alt && ch.is_ascii() && !ch.is_control() {
		out.push(0x1b);
	}
	let mut buf = [0u8; 4];
	out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
	out
}

const KITTY_DISAMBIGUATE: u8 = 1;
const KITTY_EVENT_TYPES: u8 = 2;
const KITTY_ALTERNATE: u8 = 4;
const KITTY_ALL_KEYS: u8 = 8;
const KITTY_TEXT: u8 = 16;

fn kitty_functional(vk: u16) -> Option<u32> {
	Some(match vk {
		0x1B => 27,
		0x0D => 13,
		0x09 => 9,
		0x08 => 127,
		0x2D => 57348,
		0x2E => 57349,
		0x25 => 57350,
		0x27 => 57351,
		0x26 => 57352,
		0x28 => 57353,
		0x21 => 57354,
		0x22 => 57355,
		0x24 => 57356,
		0x23 => 57357,
		0x70 => 57364,
		0x71 => 57365,
		0x72 => 57366,
		0x73 => 57367,
		0x74 => 57368,
		0x75 => 57369,
		0x76 => 57370,
		0x77 => 57371,
		0x78 => 57372,
		0x79 => 57373,
		0x7A => 57374,
		0x7B => 57375,
		_ => return None,
	})
}

pub fn encode_kitty(key: &Key, flags: u8) -> Vec<u8> {
	let mut out = Vec::new();
	if flags == 0 {
		return encode_legacy(key);
	}
	if !key.down && flags & KITTY_EVENT_TYPES == 0 {
		return out;
	}
	let mut mods = 0u32;
	if key.shift {
		mods |= 1;
	}
	if key.alt {
		mods |= 2;
	}
	if key.ctrl {
		mods |= 4;
	}
	if key.caps {
		mods |= 64;
	}
	if key.num {
		mods |= 128;
	}
	let mods = mods + 1;
	let event = if !key.down {
		":3".to_string()
	} else if key.repeat > 1 && flags & KITTY_EVENT_TYPES != 0 {
		":2".to_string()
	} else {
		String::new()
	};
	let text = if flags & KITTY_TEXT != 0 {
		key.ch
			.filter(|c| key.down)
			.map(|c| format!(";{}", c as u32))
			.unwrap_or_default()
	} else {
		String::new()
	};

	let functional = kitty_functional(key.vk);
	if let Some(code) = functional {
		let keep_legacy = flags & KITTY_ALL_KEYS == 0
			&& !key.any_mod()
			&& !matches!(code, 27)
			&& flags & KITTY_DISAMBIGUATE != 0;
		if keep_legacy {
			// Arrows still use the legacy form under disambiguate; Enter etc. too.
			return encode_legacy(key);
		}
		match key.vk {
			0x25..=0x28 => {
				let c = match key.vk {
					0x26 => 'A',
					0x28 => 'B',
					0x27 => 'C',
					_ => 'D',
				};
				let _ = write!(out, "\x1b[1;{mods}{event}{c}");
			}
			0x24 => {
				let _ = write!(out, "\x1b[1;{mods}{event}H");
			}
			0x23 => {
				let _ = write!(out, "\x1b[1;{mods}{event}F");
			}
			0x2D | 0x2E | 0x21 | 0x22 => {
				let n = match key.vk {
					0x2D => 2,
					0x2E => 3,
					0x21 => 5,
					_ => 6,
				};
				let _ = write!(out, "\x1b[{n};{mods}{event}~");
			}
			0x70..=0x73 => {
				let c = match key.vk {
					0x70 => 'P',
					0x71 => 'Q',
					0x72 => 'R',
					_ => 'S',
				};
				let _ = write!(out, "\x1b[1;{mods}{event}{c}");
			}
			0x74..=0x7B => {
				let intro = match key.vk {
					0x74 => 15,
					0x75 => 17,
					0x76 => 18,
					0x77 => 19,
					0x78 => 20,
					0x79 => 21,
					0x7A => 23,
					_ => 24,
				};
				let _ = write!(out, "\x1b[{intro};{mods}{event}~");
			}
			_ => {
				let _ = write!(out, "\x1b[{code};{mods}{event}{text}u");
			}
		}
		return out;
	}

	let Some(ch) = key.ch else {
		return out;
	};
	let plain_text = key.down
		&& !key.ctrl
		&& !key.alt
		&& flags & KITTY_ALL_KEYS == 0
		&& !(flags & KITTY_DISAMBIGUATE != 0 && ch == '\x1b');
	if plain_text {
		let mut buf = [0u8; 4];
		out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
		return out;
	}
	let cp = ch as u32;
	let alternate = if flags & KITTY_ALTERNATE != 0 && key.shift {
		format!(":{}", ch.to_ascii_uppercase() as u32)
	} else {
		String::new()
	};
	let _ = write!(out, "\x1b[{cp}{alternate};{mods}{event}{text}u");
	out
}

pub fn encode(key: &Key, win32: bool, kitty: u8, cursor_keys: bool, newline: bool) -> Vec<u8> {
	if win32 {
		return encode_win32(key).into_bytes();
	}
	if kitty != 0 {
		return encode_kitty(key, kitty);
	}
	let mut k = *key;
	k.cursor_keys = cursor_keys;
	let mut out = encode_legacy(&k);
	if newline && k.vk == 0x0D && !k.ctrl && !k.shift && !k.alt {
		out.push(b'\n');
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn k(vk: u16, ch: Option<char>) -> Key {
		Key {
			vk,
			ch,
			down: true,
			repeat: 1,
			..Key::default()
		}
	}

	#[test]
	fn ctrl_letters_are_control_bytes() {
		let mut key = k(0x43, Some('c'));
		key.ctrl = true;
		assert_eq!(encode_legacy(&key), vec![3]);
		let mut key = k(0x41, Some('a'));
		key.ctrl = true;
		assert_eq!(encode_legacy(&key), vec![1]);
	}

	#[test]
	fn alt_prefixes_escape() {
		let mut key = k(0x58, Some('x'));
		key.alt = true;
		assert_eq!(encode_legacy(&key), b"\x1bx");
	}

	#[test]
	fn arrows_use_application_mode() {
		let mut key = k(0x26, None);
		key.cursor_keys = true;
		assert_eq!(encode_legacy(&key), b"\x1bOA");
		key.cursor_keys = false;
		assert_eq!(encode_legacy(&key), b"\x1b[A");
		let mut key = k(0x26, None);
		key.ctrl = true;
		key.cursor_keys = true;
		assert_eq!(encode_legacy(&key), b"\x1b[1;5A");
	}

	#[test]
	fn win32_sequence_shape() {
		let mut key = k(0x41, Some('A'));
		key.scan = 30;
		key.shift = true;
		key.repeat = 2;
		assert_eq!(encode_win32(&key), "\x1b[65;30;65;1;16;2_");
	}

	#[test]
	fn kitty_plain_text_is_text() {
		let mut key = k(0x41, Some('a'));
		assert_eq!(encode_kitty(&key, KITTY_DISAMBIGUATE), b"a");
		key.ctrl = true;
		assert_eq!(encode_kitty(&key, KITTY_DISAMBIGUATE), b"\x1b[97;5u");
	}

	#[test]
	fn function_keys() {
		assert_eq!(encode_legacy(&k(0x70, None)), b"\x1bOP");
		let mut f5 = k(0x74, None);
		assert_eq!(encode_legacy(&f5), b"\x1b[15~");
		f5.shift = true;
		assert_eq!(encode_legacy(&f5), b"\x1b[15;2~");
		assert_eq!(encode_legacy(&k(0x7B, None)), b"\x1b[24~");
		assert_eq!(encode_kitty(&k(0x70, None), KITTY_ALL_KEYS), b"\x1b[1;1P");
		assert_eq!(encode_kitty(&k(0x74, None), KITTY_ALL_KEYS), b"\x1b[15;1~");
	}

	#[test]
	fn kitty_event_types() {
		let mut key = k(0x41, Some('a'));
		key.down = false;
		assert_eq!(
			encode_kitty(&key, KITTY_DISAMBIGUATE | KITTY_EVENT_TYPES),
			b"\x1b[97;1:3u"
		);
	}
}
