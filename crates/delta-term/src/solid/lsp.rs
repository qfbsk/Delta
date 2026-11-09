use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::highlight::Language;

pub const WM_APP_LSP: u32 = 0x8000 + 6;

#[derive(Clone, Debug)]
pub struct Diagnostic {
	pub line: usize,
	pub start: usize,
	pub end: usize,
	pub severity: u8,
	pub message: String,
}

#[derive(Clone, Debug)]
pub struct CompletionItem {
	pub label: String,
	pub insert: String,
	pub detail: String,
	pub kind: i64,
}

#[derive(Debug)]
pub enum LspEvent {
	Diagnostics(Vec<Diagnostic>),
	Completions { id: i64, items: Vec<CompletionItem> },
	Status(String),
}

pub struct LspClient {
	child: Option<Child>,
	writer: Arc<Mutex<ChildStdin>>,
	events: Receiver<LspEvent>,
	next_id: i64,
	pending: Arc<Mutex<Option<i64>>>,
	version: i64,
	uri: String,
	closed: bool,
}

impl LspClient {
	pub fn start(
		path: &Path,
		language: Language,
		text: &str,
		hwnd: usize,
	) -> Result<Self, String> {
		let (program, args) = server_command(language)
			.ok_or_else(|| format!("no language server for {}", language.name()))?;
		let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
		let root = absolute
			.parent()
			.map(Path::to_path_buf)
			.unwrap_or_else(|| Path::to_path_buf(absolute.as_path()));
		let root_uri = file_uri(&root);
		let uri = file_uri(&absolute);

		let mut command = Command::new(&program);
		command
			.args(&args)
			.current_dir(&root)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::null());
		let mut child = command
			.spawn()
			.map_err(|err| format!("cannot start {program}: {err}"))?;
		let stdin = child.stdin.take().ok_or("no stdin")?;
		let stdout = child.stdout.take().ok_or("no stdout")?;
		let writer = Arc::new(Mutex::new(stdin));
		let pending = Arc::new(Mutex::new(None));
		let (tx, rx) = mpsc::channel();

		let initialize = json!({
			"jsonrpc": "2.0",
			"id": 1,
			"method": "initialize",
			"params": {
				"processId": std::process::id(),
				"clientInfo": {"name": "solid", "version": "0.1"},
				"rootUri": root_uri,
				"capabilities": {
					"textDocument": {
						"completion": {
							"dynamicRegistration": false,
							"completionItem": {"snippetSupport": false}
						},
						"publishDiagnostics": {"relatedInformation": false},
						"synchronization": {}
					},
					"workspace": {"workspaceFolders": true},
					"window": {"workDoneProgress": true}
				},
				"workspaceFolders": [{"uri": root_uri, "name": "workspace"}]
			}
		});
		send_value(&writer, initialize);
		send_value(
			&writer,
			json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
		);
		send_value(
			&writer,
			json!({
				"jsonrpc": "2.0",
				"method": "textDocument/didOpen",
				"params": {"textDocument": {
					"uri": uri,
					"languageId": language.lsp_id(),
					"version": 1,
					"text": text
				}}
			}),
		);

		let reader_writer = writer.clone();
		let reader_pending = pending.clone();
		let reader_root = root_uri.clone();
		let reader_uri = uri.clone();
		thread::spawn(move || {
			let mut reader = BufReader::new(stdout);
			loop {
				let mut length = 0usize;
				loop {
					let mut line = String::new();
					if reader.read_line(&mut line).unwrap_or(0) == 0 {
						return;
					}
					let line = line.trim_end();
					if line.is_empty() {
						break;
					}
					if let Some(value) = line.strip_prefix("Content-Length:") {
						length = value.trim().parse().unwrap_or(0);
					}
				}
				if length == 0 {
					continue;
				}
				let mut body = vec![0u8; length];
				if reader.read_exact(&mut body).is_err() {
					return;
				}
				let value: Value = match serde_json::from_slice(&body) {
					Ok(value) => value,
					Err(_) => continue,
				};
				if let Some(method) = value["method"].as_str() {
					if let Some(id) = value.get("id") {
						let result = server_result(method, &value, &reader_root);
						send_value(
							&reader_writer,
							json!({"jsonrpc": "2.0", "id": id, "result": result}),
						);
					}
					if method == "textDocument/publishDiagnostics" {
						if value["params"]["uri"].as_str() == Some(reader_uri.as_str()) {
							let diagnostics = parse_diagnostics(&value["params"]["diagnostics"]);
							let _ = tx.send(LspEvent::Diagnostics(diagnostics));
							post(hwnd);
						}
					}
					continue;
				}
				if let Some(id) = value["id"].as_i64() {
					let expected = *reader_pending.lock().unwrap();
					if expected == Some(id) {
						let items = parse_completions(&value["result"]);
						let _ = tx.send(LspEvent::Completions { id, items });
						post(hwnd);
					}
				}
			}
		});

		Ok(Self {
			child: Some(child),
			writer,
			events: rx,
			next_id: 2,
			pending,
			version: 1,
			uri,
			closed: false,
		})
	}

	pub fn try_event(&self) -> Option<LspEvent> {
		match self.events.try_recv() {
			Ok(event) => Some(event),
			Err(_) => None,
		}
	}

	pub fn did_change(&mut self, text: &str) {
		self.version += 1;
		send_value(
			&self.writer,
			json!({
				"jsonrpc": "2.0",
				"method": "textDocument/didChange",
				"params": {
					"textDocument": {"uri": self.uri, "version": self.version},
					"contentChanges": [{"text": text}]
				}
			}),
		);
	}

	pub fn did_save(&self) {
		send_value(
			&self.writer,
			json!({
				"jsonrpc": "2.0",
				"method": "textDocument/didSave",
				"params": {"textDocument": {"uri": self.uri}}
			}),
		);
	}

	pub fn request_completion(&mut self, line: usize, character: usize) -> i64 {
		let id = self.next_id;
		self.next_id += 1;
		*self.pending.lock().unwrap() = Some(id);
		send_value(
			&self.writer,
			json!({
				"jsonrpc": "2.0",
				"id": id,
				"method": "textDocument/completion",
				"params": {
					"textDocument": {"uri": self.uri},
					"position": {"line": line, "character": character},
					"context": {"triggerKind": 1}
				}
			}),
		);
		id
	}

	pub fn shutdown(&mut self) {
		if self.closed {
			return;
		}
		self.closed = true;
		let id = self.next_id;
		self.next_id += 1;
		send_value(
			&self.writer,
			json!({"jsonrpc": "2.0", "id": id, "method": "shutdown"}),
		);
		send_value(
			&self.writer,
			json!({"jsonrpc": "2.0", "method": "exit"}),
		);
		if let Some(mut child) = self.child.take() {
			let start = Instant::now();
			while start.elapsed() < Duration::from_millis(300) {
				if child.try_wait().ok().flatten().is_some() {
					return;
				}
				thread::sleep(Duration::from_millis(20));
			}
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

impl Drop for LspClient {
	fn drop(&mut self) {
		self.shutdown();
	}
}

fn server_command(language: Language) -> Option<(String, Vec<String>)> {
	match language {
		Language::Rust => Some((String::from("rust-analyzer"), Vec::new())),
		Language::C => Some((String::from("clangd"), Vec::new())),
		Language::Python => Some((
			String::from("cmd"),
			vec![
				String::from("/C"),
				String::from("pyright-langserver"),
				String::from("--stdio"),
			],
		)),
		Language::JavaScript => Some((
			String::from("cmd"),
			vec![
				String::from("/C"),
				String::from("typescript-language-server"),
				String::from("--stdio"),
			],
		)),
		_ => None,
	}
}

fn send_value(writer: &Arc<Mutex<ChildStdin>>, value: Value) {
	let body = value.to_string();
	let mut guard = match writer.lock() {
		Ok(guard) => guard,
		Err(_) => return,
	};
	let _ = write!(guard, "Content-Length: {}\r\n\r\n{}", body.len(), body);
	let _ = guard.flush();
}

fn post(hwnd: usize) {
	if hwnd == 0 {
		return;
	}
	unsafe {
		windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
			hwnd as windows_sys::Win32::Foundation::HWND,
			WM_APP_LSP,
			0,
			0,
		);
	}
}

fn server_result(method: &str, value: &Value, root_uri: &str) -> Value {
	match method {
		"workspace/configuration" => {
			let count = value["params"]["items"].as_array().map(Vec::len).unwrap_or(0);
			Value::Array(vec![Value::Null; count])
		}
		"workspace/workspaceFolders" => {
			json!([{"uri": root_uri, "name": "workspace"}])
		}
		"workspace/applyEdit" => json!({"applied": false}),
		_ => Value::Null,
	}
}

fn parse_diagnostics(value: &Value) -> Vec<Diagnostic> {
	let mut out = Vec::new();
	for item in value.as_array().into_iter().flatten() {
		let line = item["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
		let start = item["range"]["start"]["character"].as_u64().unwrap_or(0) as usize;
		let mut end = item["range"]["end"]["character"].as_u64().unwrap_or(0) as usize;
		if end <= start {
			end = start + 1;
		}
		out.push(Diagnostic {
			line,
			start,
			end,
			severity: item["severity"].as_u64().unwrap_or(1) as u8,
			message: String::from(item["message"].as_str().unwrap_or("")),
		});
	}
	out
}

fn parse_completions(value: &Value) -> Vec<CompletionItem> {
	let items = match value {
		Value::Array(items) => items.as_slice(),
		Value::Object(map) => map
			.get("items")
			.and_then(Value::as_array)
			.map(Vec::as_slice)
			.unwrap_or(&[]),
		_ => &[],
	};
	let mut out = Vec::new();
	for item in items.iter().take(50) {
		let label = item["label"].as_str().unwrap_or("");
		if label.is_empty() {
			continue;
		}
		let insert = item["textEdit"]["newText"]
			.as_str()
			.or_else(|| item["insertText"].as_str())
			.unwrap_or(label);
		out.push(CompletionItem {
			label: String::from(label),
			insert: String::from(insert),
			detail: String::from(item["detail"].as_str().unwrap_or("")),
			kind: item["kind"].as_i64().unwrap_or(0),
		});
	}
	out
}

pub fn file_uri(path: &Path) -> String {
	let text = path.to_string_lossy().replace('\\', "/");
	let mut out = String::from("file:///");
	for byte in text.bytes() {
		match byte {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
				out.push(byte as char)
			}
			_ => out.push_str(&format!("%{byte:02X}")),
		}
	}
	out
}
