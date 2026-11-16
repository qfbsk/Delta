//! LSP client for solid. Semantics transcribed from Neovim 0.12.6 (Apache-2.0),
//! which is the pinned reference under `vendor/neovim-0.12.6`:
//!
//! - `runtime/lua/vim/lsp/rpc.lua` — `Content-Length` framing, one request id
//!   sequence, `shutdown`/`exit`.
//! - `runtime/lua/vim/lsp/client.lua` — initialize/initialized lifecycle and
//!   answering server-to-client requests.
//! - `runtime/lua/vim/lsp/sync.lua` — document versions and full-text changes.
//!   rust-analyzer asks for incremental sync, so the whole document is sent as
//!   one change spanning (0,0)..(end), which the protocol allows.
//! - `runtime/lua/vim/lsp/handlers.lua` — `textDocument/publishDiagnostics`,
//!   `workspace/configuration` (reply one null per item), workDoneProgress.
//! - `runtime/lua/vim/lsp/completion.lua` — `_lsp_to_complete_items`,
//!   trigger kinds, prefix matching and `get_completion_word` precedence.
//! - `runtime/lua/vim/lsp/util.lua` — `apply_text_edits`: reverse-sorted edits,
//!   reversed ranges fixed, `end_col` clamped to the line length.
//!
//! Positions on the wire are UTF-16 code units (the only encoding we advertise).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
	m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pos {
	pub line: u32,
	/// UTF-16 code units, as Neovim's default `offset_encoding`.
	pub character: u32,
}

#[derive(Clone, Debug)]
pub struct Edit {
	pub start: Pos,
	pub end: Pos,
	pub new_text: String,
}

#[derive(Clone, Debug)]
pub struct Item {
	pub label: String,
	pub detail: String,
	pub filter_text: Option<String>,
	pub sort_text: Option<String>,
	pub insert_text: Option<String>,
	pub text_edit: Option<Edit>,
	pub additional: Vec<Edit>,
	/// LSP `insertTextFormat`: 2 means the text is a snippet.
	pub insert_format: u8,
	/// LSP `CompletionItemKind` 1..25; 0 when the server sent none.
	pub kind: u8,
}

/// One signature from `textDocument/signatureHelp`.
#[derive(Clone, Debug)]
pub struct SigSignature {
	pub label: String,
	pub parameters: Vec<SigParam>,
	/// `activeParameter` on the signature itself wins over the help's.
	pub active_parameter: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct SigParam {
	pub label: SigParamLabel,
}

#[derive(Clone, Debug)]
pub enum SigParamLabel {
	Text(String),
	/// `[start, end)` in UTF-16 code units of the signature label.
	Offset(u32, u32),
}

#[derive(Clone, Debug)]
pub struct SigHelp {
	pub signatures: Vec<SigSignature>,
	pub active_signature: usize,
	pub active_parameter: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Diag {
	pub start: Pos,
	pub end: Pos,
	pub severity: u8,
	pub message: String,
	pub source: String,
}

struct CompletionState {
	req: u64,
	done: bool,
	items: Vec<Item>,
	incomplete: bool,
}

struct SignatureState {
	req: u64,
	done: bool,
	value: Option<SigHelp>,
}

struct State {
	next_id: u64,
	init_id: u64,
	init_done: bool,
	init_ok: bool,
	/// The reader thread hit EOF; nothing more will be answered.
	dead: bool,
	triggers: Vec<String>,
	sig_triggers: Vec<String>,
	diagnostics: HashMap<String, Vec<Diag>>,
	diag_gen: u64,
	completion: HashMap<String, CompletionState>,
	signature: HashMap<String, SignatureState>,
}

struct Inner {
	stdin: Mutex<std::process::ChildStdin>,
	child: Mutex<Child>,
	state: Mutex<State>,
	cv: Condvar,
}

pub struct Client {
	inner: Arc<Inner>,
}

impl Client {
	fn send(&self, msg: &Value) {
		send(&self.inner, msg);
	}

	fn request(&self, method: &str, params: Value) -> u64 {
		let id = {
			let mut st = lock(&self.inner.state);
			st.next_id += 1;
			st.next_id
		};
		self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
		id
	}

	fn notify(&self, method: &str, params: Value) {
		self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
	}

	pub fn trigger_chars(&self) -> Vec<String> {
		lock(&self.inner.state).triggers.clone()
	}

	pub fn signature_trigger_chars(&self) -> Vec<String> {
		lock(&self.inner.state).sig_triggers.clone()
	}

	fn signature(&self, uri: &str, req: u64) -> Option<(bool, Option<SigHelp>)> {
		let st = lock(&self.inner.state);
		let state = st.signature.get(uri)?;
		if state.req != req {
			return None;
		}
		Some((state.done, state.value.clone()))
	}

	fn completion(&self, uri: &str, req: u64) -> Option<(Vec<Item>, bool)> {
		let st = lock(&self.inner.state);
		let state = st.completion.get(uri)?;
		if state.req != req || !state.done {
			return None;
		}
		Some((state.items.clone(), state.incomplete))
	}

	fn diagnostics_if_new(&self, uri: &str, seen: u64) -> Option<(u64, Vec<Diag>)> {
		let st = lock(&self.inner.state);
		if st.diag_gen == seen {
			return None;
		}
		// Servers may normalize the drive letter (`c:/` vs `C:/`); on Windows
		// the same file is the same buffer either way.
		let mut diags = Vec::new();
		for (key, list) in &st.diagnostics {
			if uri_eq(key, uri) {
				diags.extend_from_slice(list);
			}
		}
		Some((st.diag_gen, diags))
	}

	fn alive(&self) -> bool {
		let st = lock(&self.inner.state);
		!st.dead && st.init_done && st.init_ok
	}
}

fn send(inner: &Inner, msg: &Value) {
	let Ok(body) = serde_json::to_vec(msg) else {
		return;
	};
	let mut head = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
	head.extend_from_slice(&body);
	let mut stdin = lock(&inner.stdin);
	use std::io::Write;
	let _ = stdin.write_all(&head);
	let _ = stdin.flush();
}

fn read_message<R: std::io::BufRead>(reader: &mut R) -> Option<Value> {
	let mut len = None;
	loop {
		let mut line = String::new();
		if reader.read_line(&mut line).ok()? == 0 {
			return None;
		}
		let line = line.trim_end_matches(['\r', '\n']);
		if line.is_empty() {
			break;
		}
		let (name, value) = line.split_once(':')?;
		if name.eq_ignore_ascii_case("content-length") {
			len = value.trim().parse::<usize>().ok();
		}
	}
	let mut body = vec![0u8; len?];
	reader.read_exact(&mut body).ok()?;
	serde_json::from_slice(&body).ok()
}

fn handle(inner: &Arc<Inner>, msg: Value) {
	let method = msg.get("method").and_then(|m| m.as_str());
	let id = msg.get("id").cloned();
	match (method, id) {
		(Some(method), Some(id)) => {
			// Server-to-client request. Neovim answers workDoneProgress/create and
			// friends with null; workspace/configuration gets one entry per item.
			let result = if method == "workspace/configuration" {
				let count = msg
					.pointer("/params/items")
					.and_then(|i| i.as_array())
					.map(|i| i.len())
					.unwrap_or(0);
				Value::Array(vec![Value::Null; count])
			} else {
				Value::Null
			};
			send(inner, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
		}
		(Some(method), None) => match method {
			"textDocument/publishDiagnostics" => {
				let uri = msg
					.pointer("/params/uri")
					.and_then(|u| u.as_str())
					.unwrap_or("")
					.to_string();
				let diags = msg
					.pointer("/params/diagnostics")
					.and_then(|d| d.as_array())
					.map(|a| a.iter().filter_map(diag_from).collect::<Vec<_>>())
					.unwrap_or_default();
				let mut st = lock(&inner.state);
				st.diagnostics.insert(uri, diags);
				st.diag_gen += 1;
			}
			_ => {}
		},
		(None, Some(id)) => {
			// Response to one of our requests.
			let mut st = lock(&inner.state);
			if id.as_u64() == Some(st.init_id) {
				st.init_ok = msg.get("result").is_some();
				st.triggers = string_list(msg.pointer("/result/capabilities/completionProvider/triggerCharacters"));
				st.sig_triggers = string_list(
					msg.pointer("/result/capabilities/signatureHelpProvider/triggerCharacters"),
				);
				st.init_done = true;
				inner.cv.notify_all();
				return;
			}
			let id = id.as_u64().unwrap_or(0);
			let key = st
				.completion
				.iter()
				.find(|(_, c)| c.req == id)
				.map(|(uri, _)| uri.clone());
			if let Some(uri) = key {
				let (items, incomplete) = parse_completion(msg.get("result").unwrap_or(&Value::Null));
				let is_incomplete = incomplete;
				if let Some(c) = st.completion.get_mut(&uri) {
					c.items = items;
					c.incomplete = is_incomplete;
					c.done = true;
				}
			}
			let sig_key = st
				.signature
				.iter()
				.find(|(_, s)| s.req == id)
				.map(|(uri, _)| uri.clone());
			if let Some(uri) = sig_key {
				let value = signature_help_from(msg.get("result").unwrap_or(&Value::Null));
				if let Some(s) = st.signature.get_mut(&uri) {
					s.value = value;
					s.done = true;
				}
			}
		}
		(None, None) => {}
	}
}

fn string_list(v: Option<&Value>) -> Vec<String> {
	v.and_then(|t| t.as_array())
		.map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
		.unwrap_or_default()
}

fn signature_help_from(v: &Value) -> Option<SigHelp> {
	let signatures: Vec<SigSignature> = v
		.get("signatures")?
		.as_array()?
		.iter()
		.filter_map(|s| {
			let label = s.get("label")?.as_str()?.to_string();
			let parameters = s
				.get("parameters")
				.and_then(|p| p.as_array())
				.map(|a| {
					a.iter()
						.filter_map(|p| {
							let label = p.get("label")?;
							let label = if let Some(text) = label.as_str() {
								SigParamLabel::Text(text.to_string())
							} else {
								let range = label.as_array()?;
								SigParamLabel::Offset(
									range.first()?.as_u64()? as u32,
									range.get(1)?.as_u64()? as u32,
								)
							};
							Some(SigParam { label })
						})
						.collect()
				})
				.unwrap_or_default();
			let active_parameter =
				s.get("activeParameter").and_then(|a| a.as_u64()).map(|a| a as usize);
			Some(SigSignature { label, parameters, active_parameter })
		})
		.collect();
	if signatures.is_empty() {
		return None;
	}
	Some(SigHelp {
		signatures,
		active_signature: v.get("activeSignature").and_then(|a| a.as_u64()).unwrap_or(0) as usize,
		active_parameter: v.get("activeParameter").and_then(|a| a.as_u64()).map(|a| a as usize),
	})
}

fn utf16_to_chars(text: &str, units: u32) -> usize {
	let mut seen = 0u32;
	for (i, c) in text.chars().enumerate() {
		if seen >= units {
			return i;
		}
		seen += c.len_utf16() as u32;
		if seen > units {
			return i;
		}
	}
	text.chars().count()
}

fn find_chars(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
	if needle.is_empty() || from > hay.len() {
		return Some(from);
	}
	hay.get(from..)?
		.windows(needle.len())
		.position(|w| w == needle)
		.map(|p| p + from)
}

/// The active parameter's character range inside the active signature label,
/// after Neovim `convert_signature_help_to_markdown_lines`: an explicit
/// `[start, end]` label wins, otherwise each parameter's text is located in
/// order, starting from the first trigger character.
pub fn active_parameter_range(help: &SigHelp, triggers: &[String]) -> Option<(usize, usize)> {
	let mut index = help.active_signature;
	if index >= help.signatures.len() {
		index = 0;
	}
	let signature = help.signatures.get(index)?;
	let active = signature.active_parameter.or(help.active_parameter)?;
	let param = signature.parameters.get(active)?;
	let label: Vec<char> = signature.label.chars().collect();
	match &param.label {
		SigParamLabel::Offset(start, end) => Some((
			utf16_to_chars(&signature.label, *start),
			utf16_to_chars(&signature.label, *end),
		)),
		SigParamLabel::Text(_) => {
			let mut offset = 0usize;
			for trigger in triggers {
				let t: Vec<char> = trigger.chars().collect();
				if let Some(pos) = find_chars(&label, &t, 0) {
					if offset == 0 || pos < offset {
						offset = pos;
					}
				}
			}
			for (p, candidate) in signature.parameters.iter().enumerate() {
				let SigParamLabel::Text(text) = &candidate.label else {
					break;
				};
				let needle: Vec<char> = text.chars().collect();
				let Some(pos) = find_chars(&label, &needle, offset) else {
					break;
				};
				if p == active {
					return Some((pos, pos + needle.len()));
				}
				offset = pos + needle.len() + 1;
			}
			None
		}
	}
}

fn pos_from(v: &Value) -> Option<Pos> {
	Some(Pos {
		line: v.get("line")?.as_u64()? as u32,
		character: v.get("character")?.as_u64()? as u32,
	})
}

fn edit_from(v: &Value) -> Option<Edit> {
	let new_text = v.get("newText")?.as_str()?.to_string();
	// LSP 3.17 InsertReplaceEdit: the insert range is what gets replaced.
	let range = v.get("range").or_else(|| v.get("insert"))?;
	Some(Edit {
		start: pos_from(range.get("start")?)?,
		end: pos_from(range.get("end")?)?,
		new_text,
	})
}

fn diag_from(v: &Value) -> Option<Diag> {
	let range = v.get("range")?;
	Some(Diag {
		start: pos_from(range.get("start")?)?,
		end: pos_from(range.get("end")?)?,
		severity: v.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) as u8,
		message: v.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
		source: v.get("source").and_then(|s| s.as_str()).unwrap_or("").to_string(),
	})
}

fn item_from(v: &Value, defaults: Option<&Value>) -> Option<Item> {
	let label = v.get("label")?.as_str()?.to_string();
	let mut text_edit = v.get("textEdit").and_then(edit_from);
	let insert_format = v
		.get("insertTextFormat")
		.or_else(|| defaults.and_then(|d| d.get("insertTextFormat")))
		.and_then(|f| f.as_u64())
		.unwrap_or(1) as u8;
	if text_edit.is_none() {
		if let Some(range) = defaults.and_then(|d| d.get("editRange")) {
			if range.get("range").is_some() || range.get("start").is_some() {
				let range = range.get("range").unwrap_or(range);
				let new_text = v
					.get("insertText")
					.and_then(|t| t.as_str())
					.unwrap_or(&label)
					.to_string();
				text_edit = Some(Edit {
					start: pos_from(range.get("start")?)?,
					end: pos_from(range.get("end")?)?,
					new_text,
				});
			}
		}
	}
	let additional = v
		.get("additionalTextEdits")
		.and_then(|a| a.as_array())
		.map(|a| a.iter().filter_map(edit_from).collect())
		.unwrap_or_default();
	let detail = v
		.get("detail")
		.and_then(|d| d.as_str())
		.or_else(|| v.pointer("/labelDetails/detail").and_then(|d| d.as_str()))
		.unwrap_or("")
		.replace(['\r', '\n'], " ");
	Some(Item {
		label,
		detail,
		filter_text: v.get("filterText").and_then(|f| f.as_str()).map(String::from),
		sort_text: v.get("sortText").and_then(|s| s.as_str()).map(String::from),
		insert_text: v.get("insertText").and_then(|t| t.as_str()).map(String::from),
		text_edit,
		additional,
		insert_format,
		kind: v.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u8,
	})
}

fn parse_completion(result: &Value) -> (Vec<Item>, bool) {
	let (items, incomplete, defaults) = match result {
		Value::Array(a) => (Some(a), false, None),
		Value::Object(o) => (
			o.get("items").and_then(|i| i.as_array()),
			o.get("isIncomplete").and_then(|i| i.as_bool()).unwrap_or(false),
			o.get("itemDefaults"),
		),
		_ => (None, false, None),
	};
	let mut out: Vec<Item> = items
		.map(|a| a.iter().filter_map(|i| item_from(i, defaults)).collect())
		.unwrap_or_default();
	// completion.lua sorts by sortText or label.
	out.sort_by(|a, b| {
		let ka = a.sort_text.as_deref().unwrap_or(&a.label);
		let kb = b.sort_text.as_deref().unwrap_or(&b.label);
		ka.cmp(kb)
	});
	(out, incomplete)
}

/// Text inserted when accepting an item: `textEdit.newText` > `insertText` >
/// `label` (`completion.lua` `get_completion_word`).
pub fn completion_text(item: &Item) -> String {
	if let Some(edit) = &item.text_edit {
		return edit.new_text.replace("\r\n", "\n").replace('\r', "\n");
	}
	if let Some(text) = item.insert_text.as_ref().filter(|t| !t.is_empty()) {
		return text.clone();
	}
	item.label.clone()
}

/// A snippet tab stop, as character offsets into the expanded text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnipStop {
	pub start: usize,
	pub end: usize,
}

struct RawStop {
	number: u32,
	start: usize,
	end: usize,
}

/// Expand an LSP snippet (`insertTextFormat: 2`) into plain text and the tab
/// stops VS Code walks: numeric stops ascending, `$0` last. Variables have no
/// values here, so `$name` becomes empty and `${name:default}` its default.
pub fn expand_snippet(text: &str) -> (String, Vec<SnipStop>) {
	let chars: Vec<char> = text.chars().collect();
	let mut out = String::new();
	let mut stops = Vec::new();
	parse_snippet(&chars, 0, chars.len(), &mut out, &mut stops);
	stops.sort_by_key(|s| (s.number == 0, s.number));
	(out, stops.iter().map(|s| SnipStop { start: s.start, end: s.end }).collect())
}

enum Braced {
	Stop { number: u32, body: Option<Body> },
	Variable { default: Option<Body> },
}

/// A placeholder body: either a source range to expand recursively or text
/// already decoded from a choice list.
enum Body {
	Range(usize, usize),
	Text(String),
}

fn push_body(body: Body, chars: &[char], out: &mut String, stops: &mut Vec<RawStop>) {
	match body {
		Body::Range(a, b) => parse_snippet(chars, a, b, out, stops),
		Body::Text(text) => out.push_str(&text),
	}
}

fn parse_snippet(chars: &[char], from: usize, to: usize, out: &mut String, stops: &mut Vec<RawStop>) {
	let mut i = from;
	while i < to {
		match chars[i] {
			'\\' if i + 1 < to && matches!(chars[i + 1], '$' | '}' | '\\') => {
				out.push(chars[i + 1]);
				i += 2;
			}
			'$' if i + 1 < to && chars[i + 1].is_ascii_digit() => {
				let mut j = i + 1;
				let mut number = 0u32;
				while j < to && chars[j].is_ascii_digit() {
					number = number.saturating_mul(10).saturating_add((chars[j] as u8 - b'0') as u32);
					j += 1;
				}
				let at = out.chars().count();
				stops.push(RawStop { number, start: at, end: at });
				i = j;
			}
			'$' if i + 1 < to && chars[i + 1] == '{' => match parse_braced(chars, i + 2, to) {
				Some((Braced::Stop { number, body }, next)) => {
					let start = out.chars().count();
					if let Some(body) = body {
						push_body(body, chars, out, stops);
					}
					let end = out.chars().count();
					stops.push(RawStop { number, start, end });
					i = next;
				}
				Some((Braced::Variable { default }, next)) => {
					if let Some(body) = default {
						push_body(body, chars, out, stops);
					}
					i = next;
				}
				None => {
					out.push('$');
					i += 1;
				}
			},
			c => {
				out.push(c);
				i += 1;
			}
		}
	}
}

/// Parse what follows `${`. Returns the construct and the index past its
/// closing `}`. Placeholder bodies keep their source range so nested snippets
/// are expanded recursively; `${1|a,b|}` yields its first choice as text.
fn parse_braced(chars: &[char], start: usize, to: usize) -> Option<(Braced, usize)> {
	if start >= to {
		return None;
	}
	if chars[start].is_ascii_digit() {
		let mut j = start;
		let mut number = 0u32;
		while j < to && chars[j].is_ascii_digit() {
			number = number.saturating_mul(10).saturating_add((chars[j] as u8 - b'0') as u32);
			j += 1;
		}
		if j >= to {
			return None;
		}
		match chars[j] {
			'}' => Some((Braced::Stop { number, body: None }, j + 1)),
			':' => {
				let end = matching_brace(chars, j + 1, to)?;
				Some((Braced::Stop { number, body: Some(Body::Range(j + 1, end)) }, end + 1))
			}
			'|' => {
				let end = choices_end(chars, j + 1, to)?;
				let mut choice = Vec::new();
				decode_choice(chars, j + 1, end, &mut choice);
				let choice: String = choice.into_iter().collect();
				let body = Some(Body::Text(choice));
				Some((Braced::Stop { number, body }, end + 2))
			}
			_ => None,
		}
	} else if chars[start].is_alphabetic() || chars[start] == '_' {
		let mut j = start;
		while j < to && (chars[j].is_alphanumeric() || chars[j] == '_') {
			j += 1;
		}
		if j >= to {
			return None;
		}
		match chars[j] {
			'}' => Some((Braced::Variable { default: None }, j + 1)),
			':' => {
				let end = matching_brace(chars, j + 1, to)?;
				Some((Braced::Variable { default: Some(Body::Range(j + 1, end)) }, end + 1))
			}
			_ => None,
		}
	} else {
		None
	}
}

fn matching_brace(chars: &[char], start: usize, to: usize) -> Option<usize> {
	let mut depth = 0usize;
	let mut j = start;
	while j < to {
		match chars[j] {
			'\\' => j += 2,
			'$' if j + 1 < to && chars[j + 1] == '{' => {
				depth += 1;
				j += 2;
			}
			'}' if depth == 0 => return Some(j),
			'}' => {
				depth -= 1;
				j += 1;
			}
			_ => j += 1,
		}
	}
	None
}

fn choices_end(chars: &[char], start: usize, to: usize) -> Option<usize> {
	let mut j = start;
	while j + 1 < to {
		match chars[j] {
			'\\' => j += 2,
			'|' if chars[j + 1] == '}' => return Some(j),
			_ => j += 1,
		}
	}
	None
}

fn decode_choice(chars: &[char], from: usize, to: usize, out: &mut Vec<char>) {
	let mut j = from;
	while j < to {
		if chars[j] == '\\' && j + 1 < to && matches!(chars[j + 1], ',' | '|' | '\\' | '$') {
			out.push(chars[j + 1]);
			j += 2;
		} else if chars[j] == ',' {
			return;
		} else {
			out.push(chars[j]);
			j += 1;
		}
	}
}

/// `completion.lua` `_lsp_to_complete_items` matching: `filterText` if present,
/// otherwise the label; a prefix with no alphanumerics matches everything.
/// Neovim's default `ignorecase` is off, so this is a plain prefix comparison.
pub fn item_matches(item: &Item, prefix: &str) -> bool {
	if !prefix.chars().any(|c| c.is_alphanumeric()) {
		return true;
	}
	let value = item.filter_text.as_deref().unwrap_or(&item.label);
	value.starts_with(prefix)
}

/// `completion.lua` `adjust_start_col`: the response may start before the
/// client's word boundary (ts_ls sends `.x` for a member access). The pum
/// filters from the earliest textEdit start on the current line.
pub fn response_start(items: &[Item], row: usize, line: &[char]) -> Option<usize> {
	let mut min: Option<usize> = None;
	for item in items {
		if let Some(edit) = &item.text_edit {
			if edit.start.line as usize == row {
				let col = char_col(line, edit.start.character);
				min = Some(min.map_or(col, |m| m.min(col)));
			}
		}
	}
	min
}

/// Number of UTF-16 code units before `char_col` on this line.
pub fn utf16_col(line: &[char], char_col: usize) -> u32 {
	line.iter().take(char_col).map(|c| c.len_utf16() as u32).sum()
}

/// Character index for a UTF-16 column, clamped to the line.
pub fn char_col(line: &[char], utf16: u32) -> usize {
	let mut units = 0u32;
	for (i, c) in line.iter().enumerate() {
		if units >= utf16 {
			return i;
		}
		units += c.len_utf16() as u32;
	}
	line.len()
}

/// `util.lua` `apply_text_edits`: fix reversed ranges, sort from the end of the
/// buffer backwards, then splice each edit. Newlines in `newText` normalize to
/// `\n`; `end_col` is clamped to the line length.
pub fn apply_edits(lines: &mut Vec<Vec<char>>, edits: &[Edit]) {
	if edits.is_empty() || lines.is_empty() {
		return;
	}
	let mut sorted: Vec<(usize, Edit)> = edits.iter().cloned().enumerate().collect();
	for (_, edit) in &mut sorted {
		if (edit.start.line, edit.start.character) > (edit.end.line, edit.end.character) {
			std::mem::swap(&mut edit.start, &mut edit.end);
		}
	}
	sorted.sort_by(|(ia, a), (ib, b)| {
		(b.start.line, b.start.character, *ib).cmp(&(a.start.line, a.start.character, *ia))
	});
	for (_, edit) in sorted {
		let start_row = (edit.start.line as usize).min(lines.len() - 1);
		let end_row = (edit.end.line as usize).min(lines.len() - 1);
		let start_col = char_col(&lines[start_row], edit.start.character);
		let end_col = char_col(&lines[end_row], edit.end.character);
		let text = edit.new_text.replace("\r\n", "\n").replace('\r', "\n");
		let segments: Vec<Vec<char>> = text.split('\n').map(|s| s.chars().collect()).collect();
		let mut replacement: Vec<Vec<char>> = Vec::with_capacity(segments.len());
		for (i, seg) in segments.iter().enumerate() {
			let mut line = Vec::new();
			if i == 0 {
				line.extend_from_slice(&lines[start_row][..start_col]);
			}
			line.extend_from_slice(seg);
			if i == segments.len() - 1 {
				line.extend_from_slice(&lines[end_row][end_col.min(lines[end_row].len())..]);
			}
			replacement.push(line);
		}
		if start_row <= end_row {
			lines.splice(start_row..=end_row, replacement);
		}
		if lines.is_empty() {
			lines.push(Vec::new());
		}
	}
}

/// Windows paths ignore case, so `file:///c:/x` and `file:///C:/x` name the
/// same document; servers are free to normalize the drive letter.
fn uri_eq(a: &str, b: &str) -> bool {
	a == b || cfg!(windows) && a.eq_ignore_ascii_case(b)
}

fn file_uri(path: &Path) -> String {
	// `std::fs::canonicalize` returns a verbatim path (`\\?\C:\...`) on
	// Windows; LSP servers read that as "not a file". `vim.uri_from_fname`
	// uses the plain path, so drop the prefix here.
	let mut text = path.to_string_lossy().into_owned();
	if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
		text = format!(r"\\{rest}");
	} else if let Some(rest) = text.strip_prefix(r"\\?\") {
		text = rest.to_string();
	}
	let mut out = String::from("file:///");
	for b in text.replace('\\', "/").bytes() {
		match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
				out.push(b as char)
			}
			_ => out.push_str(&format!("%{b:02X}")),
		}
	}
	out
}

fn root_for(path: &Path, markers: &[&str]) -> PathBuf {
	let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
	parent
		.ancestors()
		.find(|dir| markers.iter().any(|m| dir.join(m).exists()))
		.map(Path::to_path_buf)
		.unwrap_or(parent)
}

fn program_for(lang: &str) -> Option<(&'static str, Vec<String>, &'static [&'static str])> {
	let none = Vec::new();
	match lang {
		"rust" => Some(("rust-analyzer", none, &["Cargo.toml"])),
		"c" => Some((
			"clangd",
			clangd_args(),
			&["compile_commands.json", "compile_flags.txt", ".clangd", "CMakeLists.txt", "Makefile", ".git"],
		)),
		"python" => Some(("pyright-langserver", vec![String::from("--stdio")], &["pyproject.toml", "setup.py"])),
		"javascript" => Some((
			"typescript-language-server",
			vec![String::from("--stdio")],
			&["tsconfig.json", "jsconfig.json", "package.json"],
		)),
		"json" => Some(("vscode-json-language-server", vec![String::from("--stdio")], &[".git"])),
		_ => None,
	}
}

/// Without a compilation database clangd targets MSVC and reads the UCRT
/// headers, so `scanf` is reported as deprecated. The compiler in use is gcc:
/// the clangd config names it as the fallback compiler (`CompileFlags.Compiler`)
/// and this flag lets clangd run it for the MinGW include paths and target.
fn clangd_args() -> Vec<String> {
	match gcc_path() {
		Some(gcc) => vec![format!("--query-driver={gcc}")],
		None => Vec::new(),
	}
}

#[cfg(windows)]
fn gcc_path() -> Option<String> {
	// `--query-driver` is matched case-sensitively, while `$PATHEXT` supplies
	// `.EXE`. Take the on-disk spelling so `gcc.exe` matches the glob.
	let resolved = PathBuf::from(resolve_program("gcc")?);
	let real = std::fs::canonicalize(&resolved).unwrap_or(resolved);
	let mut text = real.to_string_lossy().into_owned();
	if let Some(rest) = text.strip_prefix(r"\\?\") {
		text = rest.to_string();
	}
	Some(text.replace('\\', "/"))
}

#[cfg(not(windows))]
fn gcc_path() -> Option<String> {
	None
}

/// Windows `CreateProcess` only appends `.exe` to a bare name, so npm shims
/// (`pyright-langserver.cmd`) are never found. `tv_to_argv` in Neovim's
/// `eval.c` resolves `argv[0]` through `$PATH` with `$PATHEXT` first
/// (`os/fs.c` `is_executable_in_path`); do the same.
#[cfg(windows)]
fn resolve_in(
	dirs: impl Iterator<Item = PathBuf>,
	program: &str,
	pathext: &str,
) -> Option<PathBuf> {
	let exts: Vec<&str> = pathext.split(';').filter(|e| !e.is_empty()).collect();
	let own_ext = Path::new(program)
		.extension()
		.map(|e| e.to_string_lossy().to_ascii_lowercase());
	let own_ext_ok = own_ext.as_deref().is_some_and(|e| {
		exts.iter().any(|x| x.trim_start_matches('.').eq_ignore_ascii_case(e))
	});
	for dir in dirs {
		if own_ext_ok {
			let candidate = dir.join(program);
			if candidate.is_file() {
				return Some(candidate);
			}
		}
		for ext in &exts {
			let candidate = dir.join(format!("{program}{ext}"));
			if candidate.is_file() {
				return Some(candidate);
			}
		}
	}
	None
}

#[cfg(windows)]
fn resolve_program(program: &str) -> Option<std::ffi::OsString> {
	let path = Path::new(program);
	if path.is_absolute() || path.components().count() > 1 {
		return None;
	}
	let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| String::from(".com;.exe;.bat;.cmd"));
	let path_env = std::env::var_os("PATH")?;
	let dirs = std::env::split_paths(&path_env);
	resolve_in(dirs, program, &pathext).map(PathBuf::into_os_string)
}

#[cfg(not(windows))]
fn resolve_program(_: &str) -> Option<std::ffi::OsString> {
	None
}

fn spawn(program: &str, args: &[String], root: &Path) -> Option<Arc<Client>> {
	let mut cmd = Command::new(resolve_program(program).unwrap_or_else(|| program.into()));
	cmd.args(args)
		.current_dir(root)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null());
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		// No console window for servers started from a GUI process.
		cmd.creation_flags(0x0800_0000);
	}
	let mut child = cmd.spawn().ok()?;
	let stdin = child.stdin.take()?;
	let stdout = child.stdout.take()?;
	let inner = Arc::new(Inner {
		stdin: Mutex::new(stdin),
		child: Mutex::new(child),
		state: Mutex::new(State {
			next_id: 1, // 1 is reserved for initialize; Neovim's rpc.lua never reuses an id.
			init_id: 1,
			init_done: false,
			init_ok: false,
			triggers: Vec::new(),
			sig_triggers: Vec::new(),
			diagnostics: HashMap::new(),
			diag_gen: 0,
			completion: HashMap::new(),
			signature: HashMap::new(),
			dead: false,
		}),
		cv: Condvar::new(),
	});
	{
		let inner = inner.clone();
		std::thread::spawn(move || {
			let mut reader = std::io::BufReader::new(stdout);
			while let Some(msg) = read_message(&mut reader) {
				handle(&inner, msg);
			}
			let mut st = lock(&inner.state);
			st.dead = true;
			st.init_done = true;
			st.init_ok = false;
			inner.cv.notify_all();
		});
	}
	let client = Client { inner: inner.clone() };
	let root_uri = file_uri(root);
	let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
	client.send(&json!({
		"jsonrpc": "2.0",
		"id": 1,
		"method": "initialize",
		"params": {
			"processId": std::process::id(),
			"clientInfo": {"name": "solid"},
			"rootUri": root_uri,
			"workspaceFolders": [{"uri": root_uri, "name": name}],
			"capabilities": {
				"general": {"positionEncodings": ["utf-16"]},
				"workspace": {"workspaceFolders": true, "configuration": true},
				"textDocument": {
					"synchronization": {"dynamicRegistration": false},
					"completion": {
						"dynamicRegistration": false,
						"contextSupport": true,
						"completionItem": {
							"snippetSupport": true,
							"documentationFormat": ["plaintext"]
						}
					},
					"signatureHelp": {
						"dynamicRegistration": false,
						"signatureInformation": {
							"documentationFormat": ["plaintext"],
							"parameterInformation": {"labelOffsetSupport": true}
						},
						"contextSupport": true
					},
					"publishDiagnostics": {"relatedInformation": false, "versionSupport": false}
				},
				"window": {"workDoneProgress": true}
			}
		}
	}));
	let deadline = Instant::now() + Duration::from_secs(20);
	let mut st = lock(&inner.state);
	while !st.init_done {
		let left = deadline.saturating_duration_since(Instant::now());
		if left.is_zero() {
			break;
		}
		let (next, _) = inner.cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner());
		st = next;
	}
	if !st.init_done || !st.init_ok {
		drop(st);
		let _ = lock(&inner.child).kill();
		return None;
	}
	drop(st);
	client.notify("initialized", json!({}));
	Some(Arc::new(client))
}

type Key = (String, PathBuf);

fn clients() -> &'static Mutex<HashMap<Key, Option<Arc<Client>>>> {
	struct Holder(Mutex<HashMap<Key, Option<Arc<Client>>>>);
	static ONCE: std::sync::OnceLock<Holder> = std::sync::OnceLock::new();
	&ONCE.get_or_init(|| Holder(Mutex::new(HashMap::new()))).0
}

fn client_for(path: &Path, lang: &str) -> Option<Arc<Client>> {
	let (program, args, markers) = program_for(lang)?;
	let root = std::fs::canonicalize(root_for(path, markers)).unwrap_or_else(|_| root_for(path, markers));
	let key = (lang.to_string(), root.clone());
	let mut map = lock(clients());
	if let Some(entry) = map.get(&key) {
		return entry.clone();
	}
	let client = spawn(program, &args, &root);
	map.insert(key, client.clone());
	client
}

/// One open buffer on a shared client. Dropping it sends didClose.
pub struct Doc {
	client: Arc<Client>,
	uri: String,
	version: i64,
	sent: String,
}

impl Doc {
	pub fn open(path: &Path, lang: &str, text: &str) -> Option<Doc> {
		let client = client_for(path, lang)?;
		if !client.alive() {
			return None;
		}
		let uri = file_uri(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
		client.notify(
			"textDocument/didOpen",
			json!({"textDocument": {
				"uri": uri, "languageId": lang, "version": 1, "text": text
			}}),
		);
		{
			let mut st = lock(&client.inner.state);
			st.completion.insert(
				uri.clone(),
				CompletionState { req: 0, done: false, items: Vec::new(), incomplete: false },
			);
			st.signature.insert(
				uri.clone(),
				SignatureState { req: 0, done: false, value: None },
			);
		}
		Some(Doc {
			client,
			uri,
			version: 1,
			sent: text.to_string(),
		})
	}

	pub fn sync(&mut self, text: &str) {
		if text == self.sent {
			return;
		}
		self.version += 1;
		// A change without a range replaces the whole document (LSP 3.17
		// TextDocumentContentChangeEvent). A range would have to describe the
		// previous text; clangd drops the draft when it does not cover it.
		self.client.notify(
			"textDocument/didChange",
			json!({
				"textDocument": {"uri": self.uri, "version": self.version},
				"contentChanges": [{"text": text}]
			}),
		);
		self.sent = text.to_string();
	}

	/// `trigger_kind`: 1 invoked, 2 trigger character, 3 for incomplete results
	/// (`protocol.CompletionTriggerKind`). `None` when the server is gone.
	pub fn complete(
		&self,
		line: usize,
		character: u32,
		trigger: Option<String>,
		trigger_kind: u8,
	) -> Option<u64> {
		if !self.client.alive() {
			return None;
		}
		let id = {
			let mut st = lock(&self.client.inner.state);
			st.next_id += 1;
			let id = st.next_id;
			if let Some(state) = st.completion.get_mut(&self.uri) {
				state.req = id;
				state.done = false;
				state.items.clear();
			}
			id
		};
		let mut context = json!({"triggerKind": trigger_kind});
		if let Some(ref ch) = trigger {
			context["triggerCharacter"] = json!(ch);
		}
		self.client.send(&json!({
			"jsonrpc": "2.0",
			"id": id,
			"method": "textDocument/completion",
			"params": {
				"textDocument": {"uri": self.uri},
				"position": {"line": line, "character": character},
				"context": context
			}
		}));
		Some(id)
	}

	pub fn completion(&self, req: u64) -> Option<(Vec<Item>, bool)> {
		self.client.completion(&self.uri, req)
	}

	pub fn trigger_chars(&self) -> Vec<String> {
		self.client.trigger_chars()
	}

	/// `trigger_kind`: 1 invoked, 2 trigger character
	/// (`protocol.SignatureHelpTriggerKind`). `None` when the server is gone.
	pub fn signature(
		&self,
		line: usize,
		character: u32,
		trigger: Option<String>,
		trigger_kind: u8,
	) -> Option<u64> {
		if !self.client.alive() {
			return None;
		}
		let id = {
			let mut st = lock(&self.client.inner.state);
			st.next_id += 1;
			let id = st.next_id;
			if let Some(state) = st.signature.get_mut(&self.uri) {
				state.req = id;
				state.done = false;
				state.value = None;
			}
			id
		};
		let mut context = json!({"triggerKind": trigger_kind});
		if let Some(ref ch) = trigger {
			context["triggerCharacter"] = json!(ch);
		}
		self.client.send(&json!({
			"jsonrpc": "2.0",
			"id": id,
			"method": "textDocument/signatureHelp",
			"params": {
				"textDocument": {"uri": self.uri},
				"position": {"line": line, "character": character},
				"context": context
			}
		}));
		Some(id)
	}

	/// `(done, signatures)` for request `req`; `None` while another request
	/// has replaced it.
	pub fn signature_help(&self, req: u64) -> Option<(bool, Option<SigHelp>)> {
		self.client.signature(&self.uri, req)
	}

	pub fn signature_trigger_chars(&self) -> Vec<String> {
		self.client.signature_trigger_chars()
	}

	pub fn diagnostics(&self, seen: u64) -> Option<(u64, Vec<Diag>)> {
		let (generation, mut diags) = self.client.diagnostics_if_new(&self.uri, seen)?;
		// `vim.diagnostic.set` order: severity, then position.
		diags.sort_by_key(|d| (d.severity, d.start.line, d.start.character));
		Some((generation, diags))
	}
}

impl Drop for Doc {
	fn drop(&mut self) {
		self.client.notify(
			"textDocument/didClose",
			json!({"textDocument": {"uri": self.uri}}),
		);
		let mut st = lock(&self.client.inner.state);
		st.completion.remove(&self.uri);
		st.signature.remove(&self.uri);
	}
}

/// Ask every client to shut down, as `rpc.lua` does on exit: `shutdown`
/// request, `exit` notification, then kill if it is still running.
pub fn shutdown_all() {
	let map = lock(clients());
	for (_, client) in map.iter() {
		if let Some(client) = client {
			client.send(&json!({"jsonrpc": "2.0", "id": u64::MAX, "method": "shutdown", "params": null}));
			client.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": null}));
		}
	}
	for (_, client) in map.iter() {
		if let Some(client) = client {
			let mut child = lock(&client.inner.child);
			let deadline = Instant::now() + Duration::from_millis(500);
			loop {
				if let Ok(Some(_)) = child.try_wait() {
					break;
				}
				if Instant::now() >= deadline {
					let _ = child.kill();
					break;
				}
				std::thread::sleep(Duration::from_millis(20));
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn edit(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> Edit {
		Edit {
			start: Pos { line: sl, character: sc },
			end: Pos { line: el, character: ec },
			new_text: text.to_string(),
		}
	}

	fn lines(text: &str) -> Vec<Vec<char>> {
		text.split('\n').map(|l| l.chars().collect()).collect()
	}

	fn text(lines: &[Vec<char>]) -> String {
		lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n")
	}

	#[test]
	fn utf16_counts_surrogate_pairs() {
		let line: Vec<char> = "a中😀b".chars().collect();
		assert_eq!(utf16_col(&line, 0), 0);
		assert_eq!(utf16_col(&line, 2), 2);
		assert_eq!(utf16_col(&line, 3), 4);
		assert_eq!(utf16_col(&line, 4), 5);
		assert_eq!(char_col(&line, 4), 3);
		assert_eq!(char_col(&line, 99), 4);
	}

	#[test]
	fn uri_encodes_non_ascii_and_spaces() {
		let uri = file_uri(Path::new(r"E:\Delta\中文 目录\a.rs"));
		assert_eq!(uri, "file:///E:/Delta/%E4%B8%AD%E6%96%87%20%E7%9B%AE%E5%BD%95/a.rs");
	}

	#[test]
	fn every_editable_language_has_a_server() {
		for (lang, program) in [
			("rust", "rust-analyzer"),
			("c", "clangd"),
			("python", "pyright-langserver"),
			("javascript", "typescript-language-server"),
			("json", "vscode-json-language-server"),
		] {
			assert_eq!(program_for(lang).expect(lang).0, program);
		}
		assert!(program_for("text").is_none());
	}

	#[cfg(windows)]
	#[test]
	fn pathext_resolution_finds_cmd_shims() {
		let dir = std::env::temp_dir().join(format!("delta-pathext-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let shim = dir.join("delta-shim-test.cmd");
		std::fs::write(&shim, "@echo off\n").unwrap();
		let found = resolve_in([dir.clone()].into_iter(), "delta-shim-test", ".EXE;.CMD");
		let found = found.expect("shim resolved");
		assert_eq!(found.parent(), Some(dir.as_path()));
		assert!(found.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("delta-shim-test.cmd"));
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn text_edits_apply_back_to_front() {
		let mut buf = lines("let n = v.len();");
		apply_edits(
			&mut buf,
			&[edit(0, 10, 0, 13, "is_empty"), edit(0, 0, 0, 0, "use std::x;\n")],
		);
		assert_eq!(text(&buf), "use std::x;\nlet n = v.is_empty();");
	}

	#[test]
	fn text_edits_fix_reversed_ranges_and_multiline_text() {
		let mut buf = lines("abc\ndef");
		apply_edits(&mut buf, &[edit(1, 2, 0, 1, "X\nY")]);
		assert_eq!(text(&buf), "aX\nYf");
	}

	#[test]
	fn completion_matching_uses_server_start_prefix() {
		let item = Item {
			label: String::from("x"),
			detail: String::new(),
			filter_text: Some(String::from(".x")),
			sort_text: None,
			insert_text: None,
			text_edit: None,
			additional: Vec::new(),
			insert_format: 1,
			kind: 0,
		};
		// The pum prefix runs from the server's textEdit start, so it carries
		// the `.` for member completions (ts_ls sends `.x`).
		assert!(item_matches(&item, "."));
		assert!(item_matches(&item, ".x"));
		assert!(!item_matches(&item, "y"));
	}

	#[test]
	fn response_start_takes_the_earliest_range() {
		let line: Vec<char> = "p.".chars().collect();
		let item = |start, end, text: &str| Item {
			label: String::from("x"),
			detail: String::new(),
			filter_text: None,
			sort_text: None,
			insert_text: None,
			text_edit: Some(edit(0, start, 0, end, text)),
			additional: Vec::new(),
			insert_format: 1,
			kind: 0,
		};
		assert_eq!(response_start(&[item(1, 2, ".x"), item(0, 0, "px")], 0, &line), Some(0));
		assert_eq!(response_start(&[item(1, 2, ".x")], 0, &line), Some(1));
		assert_eq!(response_start(&[item(1, 2, ".x")], 1, &line), None);
	}

	#[test]
	fn completion_matching_uses_filter_text_then_label() {
		let item = Item {
			label: String::from("len"),
			detail: String::new(),
			filter_text: Some(String::from("length")),
			sort_text: None,
			insert_text: None,
			text_edit: None,
			additional: Vec::new(),
			insert_format: 1,
			kind: 0,
		};
		assert!(item_matches(&item, "len"));
		assert!(item_matches(&item, ""));
		assert!(!item_matches(&item, "xyz"));
	}

	#[test]
	fn completion_text_precedence() {
		let mut item = Item {
			label: String::from("len"),
			detail: String::new(),
			filter_text: None,
			sort_text: None,
			insert_text: Some(String::from("length")),
			text_edit: None,
			additional: Vec::new(),
			insert_format: 1,
			kind: 0,
		};
		assert_eq!(completion_text(&item), "length");
		item.text_edit = Some(edit(0, 0, 0, 3, "len()"));
		assert_eq!(completion_text(&item), "len()");
		item.insert_text = None;
		item.text_edit = None;
		assert_eq!(completion_text(&item), "len");
	}

	#[test]
	fn snippet_expands_placeholders() {
		let (text, stops) = expand_snippet("printf(${1:const char *Format}, ${2:int n})");
		assert_eq!(text, "printf(const char *Format, int n)");
		assert_eq!(
			stops,
			vec![
				SnipStop { start: 7, end: 25 },
				SnipStop { start: 27, end: 32 },
			]
		);
	}

	#[test]
	fn snippet_walks_numeric_stops_with_zero_last() {
		let (text, stops) = expand_snippet("$0 x ${2:b} ${1:a}");
		assert_eq!(text, " x b a");
		assert_eq!(
			stops,
			vec![
				SnipStop { start: 5, end: 6 },
				SnipStop { start: 3, end: 4 },
				SnipStop { start: 0, end: 0 },
			]
		);
	}

	#[test]
	fn snippet_choice_takes_the_first_option() {
		let (text, stops) = expand_snippet("${1|one,two\\,three|}!");
		assert_eq!(text, "one!");
		assert_eq!(stops, vec![SnipStop { start: 0, end: 3 }]);
	}

	#[test]
	fn snippet_unescapes_dollars_braces_and_backslashes() {
		let (text, stops) = expand_snippet("\\$1 \\} \\\\");
		assert_eq!(text, "$1 } \\");
		assert!(stops.is_empty());
	}

	#[test]
	fn snippet_expands_nested_placeholders() {
		let (text, stops) = expand_snippet("${1:a ${2:b}}");
		assert_eq!(text, "a b");
		assert_eq!(
			stops,
			vec![
				SnipStop { start: 0, end: 3 },
				SnipStop { start: 2, end: 3 },
			]
		);
	}

	#[test]
	fn snippet_variable_default_is_plain_text() {
		let (text, stops) = expand_snippet("${TM_FILENAME:file.c}");
		assert_eq!(text, "file.c");
		assert!(stops.is_empty());
	}

	#[test]
	fn completion_kind_comes_from_the_server() {
		let item = item_from(&json!({"label": "printf", "kind": 3}), None).unwrap();
		assert_eq!(item.kind, 3);
		let item = item_from(&json!({"label": "x"}), None).unwrap();
		assert_eq!(item.kind, 0);
	}

	#[test]
	fn signature_help_reads_labels_and_active_parameter() {
		let help = signature_help_from(&json!({
			"signatures": [{
				"label": "int printf(const char *Format, ...)",
				"parameters": [{"label": "const char *Format"}, {"label": "..."}]
			}],
			"activeSignature": 0,
			"activeParameter": 1
		}))
		.unwrap();
		assert_eq!(help.signatures.len(), 1);
		assert_eq!(help.active_parameter, Some(1));
		assert_eq!(
			active_parameter_range(&help, &["(".to_string(), ",".to_string()]),
			Some((31, 34)),
			"the second parameter is the ellipsis"
		);
	}

	#[test]
	fn signature_help_uses_offset_labels_when_given() {
		let help = signature_help_from(&json!({
			"signatures": [{
				"label": "f(int a, int b)",
				"parameters": [{"label": [2, 7]}, {"label": [9, 14]}]
			}],
			"activeParameter": 0
		}))
		.unwrap();
		assert_eq!(active_parameter_range(&help, &[]), Some((2, 7)));
	}

	#[test]
	fn signature_help_without_parameters_has_no_range() {
		let help = signature_help_from(&json!({"signatures": [{"label": "f()"}]})).unwrap();
		assert_eq!(active_parameter_range(&help, &["(".to_string()]), None);
		assert!(signature_help_from(&json!({"signatures": []})).is_none());
		assert!(signature_help_from(&Value::Null).is_none());
	}
}
