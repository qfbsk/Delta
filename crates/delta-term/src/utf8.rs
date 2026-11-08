//! 转写自 `vendor/ghostty-1.4.0/src/terminal/UTF8Decoder.zig`（钉住 v1.4.0）。
//!
//! 上游注释说明它基于 Bjoern Hoehrmann 的 DFA UTF-8 解码器
//! （http://bjoern.hoehrmann.de/utf-8/decoder/dfa，MIT），加了"出错就替换"的行为：
//! 非法序列产生 U+FFFD，并且**只有序列的首字节被消耗**，其余字节交回调用方重试——
//! 这正是 C1 控制字节（0x9B 等）在 ground 里还能当 CSI 用的原因。

const CHAR_CLASSES: [u8; 256] = [
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
	7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
	8, 8, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
	10, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 4, 3, 3, 11, 6, 6, 6, 5, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
	8,
];

const TRANSITIONS: [u8; 108] = [
	0, 12, 24, 36, 60, 96, 84, 12, 12, 12, 48, 72, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
	12, 0, 12, 12, 12, 12, 12, 0, 12, 0, 12, 12, 12, 24, 12, 12, 12, 12, 12, 24, 12, 24, 12, 12,
	12, 12, 12, 12, 12, 12, 12, 24, 12, 12, 12, 12, 12, 24, 12, 12, 12, 12, 12, 12, 12, 24, 12, 12,
	12, 12, 12, 12, 12, 12, 12, 36, 12, 36, 12, 12, 12, 36, 12, 12, 12, 12, 12, 36, 12, 36, 12, 12,
	12, 36, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
];

const ACCEPT_STATE: u8 = 0;
const REJECT_STATE: u8 = 12;

#[derive(Debug, Default, Clone, Copy)]
pub struct Utf8Decoder {
	accumulator: u32,
	state: u8,
}

impl Utf8Decoder {
	/// 返回（解出的字符或 None，这个字节是否被消耗）。
	/// 没被消耗时调用方有责任用同一个字节再调一次别的路径。
	pub fn next(&mut self, byte: u8) -> (Option<char>, bool) {
		let class = CHAR_CLASSES[byte as usize];
		let initial_state = self.state;

		if self.state != ACCEPT_STATE {
			self.accumulator = (self.accumulator << 6) | u32::from(byte & 0x3f);
		} else {
			self.accumulator = (0xffu32 >> class) & u32::from(byte);
		}

		self.state = TRANSITIONS[(self.state as usize) + (class as usize)];

		if self.state == ACCEPT_STATE {
			let codepoint = self.accumulator;
			self.accumulator = 0;
			(char::from_u32(codepoint), true)
		} else if self.state == REJECT_STATE {
			self.accumulator = 0;
			self.state = ACCEPT_STATE;
			(Some('\u{FFFD}'), initial_state == ACCEPT_STATE)
		} else {
			(None, true)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// 三个测试都转写自 UTF8Decoder.zig 的同名 `test "..."`。

	#[test]
	fn ascii() {
		let mut d = Utf8Decoder::default();
		let mut out = String::new();
		for byte in "Hello, World!".bytes() {
			let (cp, consumed) = d.next(byte);
			assert!(consumed);
			if let Some(cp) = cp {
				out.push(cp);
			}
		}
		assert_eq!(out, "Hello, World!");
	}

	#[test]
	fn well_formed_utf8() {
		let mut d = Utf8Decoder::default();
		let mut out = Vec::new();
		for byte in "😄✤ÁA".bytes() {
			let mut guard = 0;
			loop {
				let (cp, consumed) = d.next(byte);
				if let Some(cp) = cp {
					out.push(cp as u32);
				}
				if consumed {
					break;
				}
				guard += 1;
				assert!(guard < 8);
			}
		}
		assert_eq!(out, vec![0x1F604, 0x2724, 0xC1, 0x41]);
	}

	#[test]
	fn partially_invalid_utf8() {
		let mut d = Utf8Decoder::default();
		let mut out = Vec::new();
		// 上游喂的是 "\xF0\x9F😄\xED\xA0\x80"：非法截断、合法表情、非法代理对。
		for byte in [0xF0u8, 0x9F, 0xF0, 0x9F, 0x98, 0x84, 0xED, 0xA0, 0x80] {
			let mut guard = 0;
			loop {
				let (cp, consumed) = d.next(byte);
				if let Some(cp) = cp {
					out.push(cp as u32);
				}
				if consumed {
					break;
				}
				guard += 1;
				assert!(guard < 8);
			}
		}
		assert_eq!(out, vec![0xFFFD, 0x1F604, 0xFFFD, 0xFFFD, 0xFFFD]);
	}
}
