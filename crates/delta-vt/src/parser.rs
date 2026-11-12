//! DEC ANSI parser, transcribed from ghostty v1.4.0 `src/terminal/Parser.zig`
//! and `src/terminal/UTF8Decoder.zig` (MIT), with the same state transitions
//! described on vt100.net (`dec_ansi_parser`).

pub const MAX_INTERMEDIATE: usize = 4;
pub const MAX_PARAMS: usize = 32;
const MAX_STRING: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
	Ground,
	Escape,
	EscapeIntermediate,
	CsiEntry,
	CsiParam,
	CsiIntermediate,
	CsiIgnore,
	DcsEntry,
	DcsParam,
	DcsIntermediate,
	DcsPassthrough,
	DcsIgnore,
	OscString,
	SosPmApcString,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Csi {
	pub intermediates: Vec<u8>,
	pub params: Vec<u16>,
	/// `seps[i]` is true when parameter `i` is followed by a colon.
	pub seps: Vec<bool>,
	pub final_byte: u8,
}

impl Csi {
	pub fn has(&self, b: u8) -> bool {
		self.intermediates.iter().any(|&x| x == b)
	}
	pub fn private(&self) -> bool {
		self.has(b'?')
	}
	pub fn param(&self, i: usize, default: u16) -> u16 {
		match self.params.get(i) {
			Some(&0) | None => default,
			Some(&v) => v,
		}
	}
	pub fn raw(&self, i: usize) -> u16 {
		self.params.get(i).copied().unwrap_or(0)
	}
	/// True when a colon follows parameter index `i`.
	pub fn colon_after(&self, i: usize) -> bool {
		self.seps.get(i).copied().unwrap_or(false)
	}
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dcs {
	pub intermediates: Vec<u8>,
	pub params: Vec<u16>,
	pub seps: Vec<bool>,
	pub final_byte: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
	Print(char),
	Execute(u8),
	Csi(Csi),
	Esc { intermediates: Vec<u8>, final_byte: u8 },
	Osc(Vec<u8>),
	DcsHook(Dcs),
	DcsPut(u8),
	DcsUnhook,
	ApcStart,
	ApcPut(u8),
	ApcEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transition {
	None,
	Ignore,
	Print,
	Execute,
	Collect,
	Param,
	EscDispatch,
	CsiDispatch,
	Put,
	OscPut,
	ApcPut,
}

/// Björn Höhrmann style UTF-8 decoder. Invalid bytes become U+FFFD.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
	buf: [u8; 4],
	len: u8,
	need: u8,
}

impl Utf8Decoder {
	pub fn reset(&mut self) {
		self.len = 0;
		self.need = 0;
	}

	/// Feed one byte. Returns a codepoint when a sequence completed.
	pub fn next(&mut self, b: u8) -> Option<char> {
		if self.need == 0 {
			if b < 0x80 {
				return Some(b as char);
			}
			let need = match b {
				0xC2..=0xDF => 1,
				0xE0..=0xEF => 2,
				0xF0..=0xF4 => 3,
				_ => {
					return Some('\u{FFFD}');
				}
			};
			self.buf[0] = b;
			self.len = 1;
			self.need = need;
			return None;
		}
		if !(0x80..=0xBF).contains(&b) {
			// Truncated sequence; report a replacement char, then reprocess
			// this byte as a potential new sequence.
			self.reset();
			return Some('\u{FFFD}');
		}
		self.buf[self.len as usize] = b;
		self.len += 1;
		if self.len == self.need + 1 {
			let n = self.len as usize;
			let cp = match std::str::from_utf8(&self.buf[..n]) {
				Ok(s) => s.chars().next(),
				Err(_) => None,
			};
			self.reset();
			return Some(cp.unwrap_or('\u{FFFD}'));
		}
		None
	}
}

pub struct Parser {
	state: State,
	intermediates: [u8; MAX_INTERMEDIATE],
	n_inter: usize,
	params: [u16; MAX_PARAMS],
	seps: [bool; MAX_PARAMS],
	n_params: usize,
	param_acc: u32,
	param_started: bool,
	osc_buf: Vec<u8>,
	utf8: Utf8Decoder,
}

impl Parser {
	pub fn new() -> Self {
		Self {
			state: State::Ground,
			intermediates: [0; MAX_INTERMEDIATE],
			n_inter: 0,
			params: [0; MAX_PARAMS],
			seps: [false; MAX_PARAMS],
			n_params: 0,
			param_acc: 0,
			param_started: false,
			osc_buf: Vec::new(),
			utf8: Utf8Decoder::default(),
		}
	}

	pub fn state(&self) -> State {
		self.state
	}

	pub fn reset(&mut self) {
		self.state = State::Ground;
		self.clear();
		self.utf8.reset();
	}

	fn clear(&mut self) {
		self.n_inter = 0;
		self.n_params = 0;
		self.seps = [false; MAX_PARAMS];
		self.param_acc = 0;
		self.param_started = false;
	}

	fn collect(&mut self, c: u8) {
		if self.n_inter < MAX_INTERMEDIATE {
			self.intermediates[self.n_inter] = c;
			self.n_inter += 1;
		}
	}

	fn param(&mut self, c: u8) {
		if c == b';' || c == b':' {
			if self.n_params < MAX_PARAMS {
				self.params[self.n_params] = self.param_acc.min(u16::MAX as u32) as u16;
				self.seps[self.n_params] = c == b':';
				self.n_params += 1;
			}
			self.param_acc = 0;
			self.param_started = false;
		} else {
			self.param_acc = self.param_acc.saturating_mul(10).saturating_add((c - b'0') as u32);
			self.param_started = true;
		}
	}

	fn finish_params(&mut self) {
		if self.param_started && self.n_params < MAX_PARAMS {
			self.params[self.n_params] = self.param_acc.min(u16::MAX as u32) as u16;
			self.seps[self.n_params] = false;
			self.n_params += 1;
		}
		self.param_acc = 0;
		self.param_started = false;
	}

	fn inter_vec(&self) -> Vec<u8> {
		self.intermediates[..self.n_inter].to_vec()
	}

	fn param_vec(&self) -> (Vec<u16>, Vec<bool>) {
		(self.params[..self.n_params].to_vec(), self.seps[..self.n_params].to_vec())
	}

	fn next_state(&self, c: u8) -> State {
		use State::*;
		match self.state {
			Ground => match c {
				0x1B => Escape,
				_ => Ground,
			},
			Escape => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Escape,
				0x20..=0x2F => EscapeIntermediate,
				0x50 => DcsEntry,
				0x58 | 0x5E | 0x5F => SosPmApcString,
				0x5B => CsiEntry,
				0x5D => OscString,
				0x7F => Escape,
				_ => Ground,
			},
			EscapeIntermediate => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => EscapeIntermediate,
				0x20..=0x2F => EscapeIntermediate,
				0x7F => EscapeIntermediate,
				_ => Ground,
			},
			CsiEntry => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => CsiEntry,
				0x20..=0x2F => CsiIntermediate,
				0x30..=0x3F => CsiParam,
				0x7F => CsiEntry,
				_ => Ground,
			},
			CsiParam => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => CsiParam,
				0x20..=0x2F => CsiIntermediate,
				0x30..=0x39 | 0x3A..=0x3B => CsiParam,
				0x3C..=0x3F => CsiIgnore,
				0x7F => CsiParam,
				_ => Ground,
			},
			CsiIntermediate => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => CsiIntermediate,
				0x20..=0x2F => CsiIntermediate,
				0x30..=0x3F => CsiIgnore,
				0x7F => CsiIntermediate,
				_ => Ground,
			},
			CsiIgnore => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => CsiIgnore,
				0x20..=0x3F => CsiIgnore,
				0x7F => CsiIgnore,
				_ => Ground,
			},
			DcsEntry => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => DcsEntry,
				0x20..=0x2F => DcsIntermediate,
				0x30..=0x3F => DcsParam,
				0x7F => DcsEntry,
				_ => DcsPassthrough,
			},
			DcsParam => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => DcsParam,
				0x20..=0x2F => DcsIntermediate,
				0x30..=0x39 | 0x3A..=0x3B => DcsParam,
				0x3C..=0x3F => DcsIgnore,
				0x7F => DcsParam,
				_ => DcsPassthrough,
			},
			DcsIntermediate => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => DcsIntermediate,
				0x20..=0x2F => DcsIntermediate,
				0x30..=0x3F => DcsIgnore,
				0x7F => DcsIntermediate,
				_ => DcsPassthrough,
			},
			DcsPassthrough => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => DcsPassthrough,
				0x7F => DcsPassthrough,
				_ => DcsPassthrough,
			},
			DcsIgnore => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => DcsIgnore,
				_ => DcsIgnore,
			},
			OscString => match c {
				0x1B => Escape,
				0x07 => Ground,
				0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => OscString,
				_ => OscString,
			},
			SosPmApcString => match c {
				0x1B => Escape,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => SosPmApcString,
				0x7F => SosPmApcString,
				_ => SosPmApcString,
			},
		}
	}

	fn transition(&self, c: u8) -> Transition {
		use State::*;
		use Transition::*;
		if c == 0x18 || c == 0x1A {
			return Execute;
		}
		match self.state {
			Ground => match c {
				0x1B => None,
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Execute,
				0x7F => Ignore,
				_ => Print,
			},
			Escape => match c {
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Execute,
				0x20..=0x2F => Collect,
				0x30..=0x4F | 0x51..=0x57 | 0x59 | 0x5A | 0x5C | 0x60..=0x7E => EscDispatch,
				_ => None,
			},
			EscapeIntermediate => match c {
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Execute,
				0x20..=0x2F => Collect,
				0x30..=0x7E => EscDispatch,
				_ => None,
			},
			CsiEntry => match c {
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Execute,
				0x20..=0x2F => Collect,
				0x30..=0x3B => Param,
				0x3C..=0x3F => Collect,
				0x40..=0x7E => CsiDispatch,
				_ => None,
			},
			CsiParam => match c {
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Execute,
				0x20..=0x2F => Collect,
				0x30..=0x3B => Param,
				0x40..=0x7E => CsiDispatch,
				_ => None,
			},
			CsiIntermediate => match c {
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Execute,
				0x20..=0x2F => Collect,
				0x40..=0x7E => CsiDispatch,
				_ => None,
			},
			CsiIgnore => match c {
				0x00..=0x17 | 0x19 | 0x1C..=0x1F => Execute,
				_ => None,
			},
			DcsEntry | DcsParam | DcsIntermediate => match c {
				0x18 | 0x1A => Execute,
				0x20..=0x2F => Collect,
				0x30..=0x3B => Param,
				0x3C..=0x3F if self.state == DcsEntry => Collect,
				_ => None,
			},
			DcsPassthrough => match c {
				0x18 | 0x1A => Execute,
				_ => Put,
			},
			DcsIgnore => match c {
				0x18 | 0x1A => Execute,
				_ => None,
			},
			OscString => match c {
				0x07 => None,
				0x18 | 0x1A => Execute,
				_ => OscPut,
			},
			SosPmApcString => match c {
				0x18 | 0x1A => Execute,
				0x00..=0x1F | 0x7F => None,
				_ => ApcPut,
			},
		}
	}

	/// Returns exit action, transition action, entry action (in that order).
	pub fn next(&mut self, c: u8) -> [Option<Action>; 3] {
		let old = self.state;
		if old == State::Ground && c >= 0x80 {
			return [None, self.utf8.next(c).map(Action::Print), None];
		}
		let new = self.next_state(c);
		let action = self.transition(c);
		let mut out: [Option<Action>; 3] = [None, None, None];

		if new != old {
			out[0] = match old {
				State::OscString => Some(Action::Osc(std::mem::take(&mut self.osc_buf))),
				State::DcsPassthrough => Some(Action::DcsUnhook),
				State::SosPmApcString => Some(Action::ApcEnd),
				_ => None,
			};
		}

		out[1] = match action {
			Transition::None | Transition::Ignore => None,
			Transition::Print => Some(Action::Print(c as char)),
			Transition::Execute => Some(Action::Execute(c)),
			Transition::Collect => {
				self.collect(c);
				None
			}
			Transition::Param => {
				self.param(c);
				None
			}
			Transition::EscDispatch => {
				self.finish_params();
				Some(Action::Esc { intermediates: self.inter_vec(), final_byte: c })
			}
			Transition::CsiDispatch => {
				self.finish_params();
				let (params, seps) = self.param_vec();
				// Colon sub-parameters are only meaningful for SGR.
				if c == b'm' || !seps.iter().any(|&s| s) {
					Some(Action::Csi(Csi {
						intermediates: self.inter_vec(),
						params,
						seps,
						final_byte: c,
					}))
				} else {
					None
				}
			}
			Transition::Put => Some(Action::DcsPut(c)),
			Transition::OscPut => {
				if self.osc_buf.len() < MAX_STRING {
					self.osc_buf.push(c);
				}
				None
			}
			Transition::ApcPut => Some(Action::ApcPut(c)),
		};

		if new != old {
			out[2] = match new {
				State::Escape | State::CsiEntry | State::DcsEntry => {
					self.clear();
					None
				}
				State::OscString => {
					self.osc_buf.clear();
					None
				}
				State::DcsPassthrough => {
					self.finish_params();
					let (params, seps) = self.param_vec();
					Some(Action::DcsHook(Dcs {
						intermediates: self.inter_vec(),
						params,
						seps,
						final_byte: c,
					}))
				}
				State::SosPmApcString => Some(Action::ApcStart),
				_ => None,
			};
			self.state = new;
		}
		out
	}
}

impl Default for Parser {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn actions(p: &mut Parser, bytes: &[u8]) -> Vec<Action> {
		let mut v = Vec::new();
		for &b in bytes {
			for a in p.next(b).into_iter().flatten() {
				v.push(a);
			}
		}
		v
	}

	#[test]
	fn plain_and_c0() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"a\x07b");
		assert!(matches!(a[0], Action::Print('a')));
		assert!(matches!(a[1], Action::Execute(0x07)));
		assert!(matches!(a[2], Action::Print('b')));
		assert_eq!(p.state(), State::Ground);
	}

	#[test]
	fn esc_charset() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"\x1b(B");
		match &a[0] {
			Action::Esc { intermediates, final_byte } => {
				assert_eq!(final_byte, &b'B');
				assert_eq!(intermediates, &vec![b'(']);
			}
			other => panic!("{other:?}"),
		}
		assert_eq!(p.state(), State::Ground);
	}

	#[test]
	fn csi_cup() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"\x1b[1;4H");
		match &a[0] {
			Action::Csi(c) => {
				assert_eq!(c.final_byte, b'H');
				assert_eq!(c.params, vec![1, 4]);
				assert!(!c.private());
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn csi_private_and_intermediate() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"\x1b[?2026$p");
		match &a[0] {
			Action::Csi(c) => {
				assert!(c.has(b'?'));
				assert!(c.has(b'$'));
				assert_eq!(c.final_byte, b'p');
				assert_eq!(c.params, vec![2026]);
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn csi_colon_sgr() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"\x1b[38:2::255:0:12m");
		match &a[0] {
			Action::Csi(c) => {
				assert_eq!(c.final_byte, b'm');
				assert_eq!(c.params, vec![38, 2, 0, 255, 0, 12]);
				assert!(c.colon_after(0));
				assert!(c.colon_after(1));
				assert!(c.colon_after(2));
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn osc_bel_and_st() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"\x1b]2;hello\x07");
		match &a[0] {
			Action::Osc(s) => assert_eq!(s, b"2;hello"),
			other => panic!("{other:?}"),
		}
		let a = actions(&mut p, b"\x1b]0;x\x1b\\");
		match &a[0] {
			Action::Osc(s) => assert_eq!(s, b"0;x"),
			other => panic!("{other:?}"),
		}
		assert_eq!(p.state(), State::Ground);
	}

	#[test]
	fn dcs_and_apc_ignored() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"\x1bP1$qm\x1b\\");
		assert!(matches!(a[0], Action::DcsHook(_)));
		assert!(a.iter().any(|x| matches!(x, Action::DcsUnhook)));
		let a = actions(&mut p, b"\x1b_Gi=1;AAAA\x1b\\");
		assert!(matches!(a[0], Action::ApcStart));
		assert!(a.iter().any(|x| matches!(x, Action::ApcEnd)));
	}

	#[test]
	fn utf8_split() {
		let mut p = Parser::new();
		let bytes = "中".as_bytes();
		assert_eq!(p.next(bytes[0]).into_iter().flatten().count(), 0);
		assert_eq!(p.next(bytes[1]).into_iter().flatten().count(), 0);
		let a: Vec<_> = p.next(bytes[2]).into_iter().flatten().collect();
		assert!(matches!(a[0], Action::Print('中')));
		let a = actions(&mut p, &[0xFF]);
		assert!(matches!(a[0], Action::Print('\u{FFFD}')));
	}

	#[test]
	fn csi_ignored_after_bad_intermediate() {
		let mut p = Parser::new();
		let a = actions(&mut p, b"[?1;2H");
		assert!(a.iter().any(|x| matches!(x, Action::Execute(0x18))));
		assert!(a.iter().all(|x| !matches!(x, Action::Csi(_))));
		assert_eq!(p.state(), State::Ground);
	}
}
