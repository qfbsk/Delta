//! 转义序列的增量解析：字节流进来，动作出去。
//!
//! 这是 conformance 的第一层（`laws/delta-reasoning.md`：vttest / esctest2 / xterm ctlseqs
//! 是必须能被对着测的地板）。解析器必须是增量的——ConPTY 会把一个序列拆成几段送进来。

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Csi {
	/// `<` `=` `>` `?` 之一，按原字节记。
	pub private: Option<u8>,
	pub params: Vec<u16>,
	pub intermediate: Option<u8>,
	pub final_byte: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
	Print(char),
	C0(u8),
	Csi(Csi),
	Osc(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
	Ground,
	Escape,
	Csi,
	Osc,
	OscEscape,
}

#[derive(Debug, Default)]
pub struct Parser {
	state: State,
	csi: Csi,
	osc: Vec<u8>,
	utf8: Vec<u8>,
}

const OSC_LIMIT: usize = 4096;

impl Default for State {
	fn default() -> Self {
		State::Ground
	}
}

impl Parser {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn advance(&mut self, byte: u8, out: &mut Vec<Action>) {
		match self.state {
			State::Ground => self.ground(byte, out),
			State::Escape => self.escape(byte, out),
			State::Csi => self.csi(byte, out),
			State::Osc => self.osc(byte, out),
			State::OscEscape => self.osc_escape(byte, out),
		}
	}

	pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Action>) {
		for byte in bytes {
			self.advance(*byte, out);
		}
	}

	fn ground(&mut self, byte: u8, out: &mut Vec<Action>) {
		match byte {
			0x1b => {
				self.utf8.clear();
				self.state = State::Escape;
			}
			0x00..=0x1f | 0x7f => out.push(Action::C0(byte)),
			0x20..=0x7e => out.push(Action::Print(byte as char)),
			_ => {
				self.utf8.push(byte);
				match std::str::from_utf8(&self.utf8) {
					Ok(text) => {
						for ch in text.chars() {
							out.push(Action::Print(ch));
						}
						self.utf8.clear();
					}
					Err(err) if err.error_len().is_some() => self.utf8.clear(),
					Err(_) => {
						if self.utf8.len() >= 4 {
							self.utf8.clear();
						}
					}
				}
			}
		}
	}

	fn escape(&mut self, byte: u8, _out: &mut Vec<Action>) {
		match byte {
			b'[' => {
				self.csi = Csi::default();
				self.state = State::Csi;
			}
			// OSC 与 DCS/APC/PM 都先当字符串收着；后三者目前只丢弃。
			b']' | b'P' | b'_' | b'^' => {
				self.osc.clear();
				self.state = State::Osc;
			}
			0x18 | 0x1a => self.state = State::Ground,
			0x1b => self.state = State::Escape,
			// ESC 7/8 的保存/恢复光标、ESC c 的复位等留给后面的切片。
			_ => self.state = State::Ground,
		}
	}

	fn csi(&mut self, byte: u8, out: &mut Vec<Action>) {
		match byte {
			b'0'..=b'9' => {
				if self.csi.params.is_empty() {
					self.csi.params.push(0);
				}
				let last = self.csi.params.last_mut().unwrap();
				*last = last.saturating_mul(10).saturating_add((byte - b'0') as u16);
			}
			b';' | b':' => self.csi.params.push(0),
			// 私有标记只可能出现在第一个参数之前。
			0x3c..=0x3f if self.csi.private.is_none() && self.csi.params.is_empty() => {
				self.csi.private = Some(byte);
			}
			0x20..=0x2f => self.csi.intermediate = Some(byte),
			0x40..=0x7e => {
				self.csi.final_byte = byte;
				let action = std::mem::take(&mut self.csi);
				out.push(Action::Csi(action));
				self.state = State::Ground;
			}
			0x1b => self.state = State::Escape,
			0x18 | 0x1a => self.state = State::Ground,
			_ => {}
		}
	}

	fn osc(&mut self, byte: u8, out: &mut Vec<Action>) {
		match byte {
			0x07 => self.finish_osc(out),
			0x1b => self.state = State::OscEscape,
			_ => {
				if self.osc.len() < OSC_LIMIT {
					self.osc.push(byte);
				}
			}
		}
	}

	fn osc_escape(&mut self, byte: u8, out: &mut Vec<Action>) {
		if byte == b'\\' {
			self.finish_osc(out);
		} else {
			// 不是 ST：把 ESC 本身还原进字符串，继续收。
			if self.osc.len() + 1 < OSC_LIMIT {
				self.osc.push(0x1b);
				self.osc.push(byte);
			}
			self.state = State::Osc;
		}
	}

	fn finish_osc(&mut self, out: &mut Vec<Action>) {
		let payload = std::mem::take(&mut self.osc);
		out.push(Action::Osc(payload));
		self.state = State::Ground;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse_all(bytes: &[u8]) -> Vec<Action> {
		let mut parser = Parser::new();
		let mut out = Vec::new();
		parser.feed(bytes, &mut out);
		out
	}

	#[test]
	fn private_markers_and_params_are_kept_apart() {
		let actions = parse_all(b"\x1b[?25l");
		assert_eq!(
			actions,
			vec![Action::Csi(Csi {
				private: Some(b'?'),
				params: vec![25],
				intermediate: None,
				final_byte: b'l'
			})]
		);
	}

	#[test]
	fn empty_params_and_explicit_zero_params_are_recorded_differently() {
		let actions = parse_all(b"\x1b[H\x1b[0;0H");
		let Csi { params, .. } = match &actions[0] {
			Action::Csi(csi) => csi.clone(),
			_ => unreachable!(),
		};
		assert!(params.is_empty());
		let Csi { params, .. } = match &actions[1] {
			Action::Csi(csi) => csi.clone(),
			_ => unreachable!(),
		};
		assert_eq!(params, vec![0, 0]);
	}

	#[test]
	fn an_unknown_final_byte_is_reported_not_swallowed() {
		let actions = parse_all(b"\x1b[1;2;3;4z");
		assert!(matches!(
			&actions[0],
			Action::Csi(Csi {
				final_byte: b'z',
				..
			})
		));
	}

	#[test]
	fn esc_restarts_a_sequence_instead_of_leaking_it_as_text() {
		let actions = parse_all(b"\x1b[12\x1b[K");
		assert_eq!(actions.len(), 1, "{actions:?}");
		assert!(matches!(
			&actions[0],
			Action::Csi(Csi {
				final_byte: b'K',
				..
			})
		));
	}

	#[test]
	fn osc_ends_at_bel_or_string_terminator() {
		assert_eq!(
			parse_all(b"\x1b]0;t\x07"),
			vec![Action::Osc(b"0;t".to_vec())]
		);
		assert_eq!(
			parse_all(b"\x1b]0;t\x1b\\"),
			vec![Action::Osc(b"0;t".to_vec())]
		);
	}

	#[test]
	fn dcs_is_collected_and_dropped_like_osc() {
		assert_eq!(
			parse_all(b"\x1bPq#0;2;0;0;0\x1b\\"),
			vec![Action::Osc(b"q#0;2;0;0;0".to_vec())]
		);
	}
}
